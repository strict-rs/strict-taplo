//! Thread-safe language-server world state and explicit workspace topology.

use std::str;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use arc_swap::ArcSwap;
use lsp_async_stub::Context;
use lsp_async_stub::RequestWriter;
use lsp_async_stub::util::Mapper;
use regex::Regex;
use serde_json::json;
use taplo::dom::Node;
use taplo::parser::Parse;
use taplo_common::AsyncRwLock;
use taplo_common::HashMap;
use taplo_common::IndexMap;
use taplo_common::config::Config;
use taplo_common::environment::Environment;
use taplo_common::schema::Schemas;
use taplo_common::schema::associations::AssociationRule;
use taplo_common::schema::associations::SchemaAssociation;
use taplo_common::schema::associations::priority;
use taplo_common::schema::associations::source;
use url::Url;

use crate::config::InitConfig;
use crate::config::LspConfig;
use crate::lsp_ext::notification::DidChangeSchemaAssociation;
use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;

/// Shared world handle used by the server context.
pub type World<E> = Arc<WorldState<E>>;

/// Send precomputed association notifications after all workspace locks are released.
pub(crate) async fn send_association_notifications<E: Environment>(
  mut context: Context<World<E>>,
  notifications: Vec<DidChangeSchemaAssociationParams>,
) {
  for notification in notifications {
    if let Err(error) = context
      .write_notification::<DidChangeSchemaAssociation, _>(Some(notification))
      .await
    {
      tracing::error!(%error, "failed to write schema association notification");
    }
  }
}

/// Per-workspace lock handle; topology and workspace state have separate ownership.
pub(crate) type WorkspaceHandle<E> = Arc<AsyncRwLock<WorkspaceState<E>>>;

/// Domain identity of a workspace.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum WorkspaceRoot {
  /// Permanent workspace for documents outside every configured root.
  Detached,
  /// Workspace owned by a real root URL.
  Rooted(Url),
}

impl WorkspaceRoot {
  /// Return the real root URL, if this is a rooted workspace.
  fn url(&self) -> Option<&Url> {
    match self {
      Self::Detached => None,
      Self::Rooted(url) => Some(url),
    }
  }
}

/// Workspace topology with an explicit detached state and insertion-ordered real roots.
pub(crate) struct Workspaces<E: Environment> {
  /// Always-present detached workspace.
  detached: WorkspaceHandle<E>,
  /// Real rooted workspaces.
  rooted:   IndexMap<Url, WorkspaceHandle<E>>,
}

impl<E: Environment> Workspaces<E> {
  /// Construct topology around the permanent detached workspace.
  fn new(detached: WorkspaceHandle<E>) -> Self {
    Self {
      detached,
      rooted: IndexMap::default(),
    }
  }

  /// Select the deepest real root containing `document`, otherwise detached.
  pub(crate) fn workspace_for_document(&self, document: &Url) -> WorkspaceHandle<E> {
    self
      .rooted
      .iter()
      .filter(|(root, _)| root_contains_document(root, document))
      .max_by_key(|(root, _)| root.path().len())
      .map_or_else(|| self.detached.clone(), |(_, handle)| handle.clone())
  }

  /// Return detached plus every rooted workspace handle.
  pub(crate) fn all_handles(&self) -> Vec<WorkspaceHandle<E>> {
    std::iter::once(self.detached.clone())
      .chain(self.rooted.values().cloned())
      .collect()
  }

  /// Return every real root and its handle in insertion order.
  pub(crate) fn rooted_handles(&self) -> Vec<(Url, WorkspaceHandle<E>)> {
    self.rooted.iter().map(|(url, handle)| (url.clone(), handle.clone())).collect()
  }

  /// Return every real root URL in insertion order.
  pub(crate) fn rooted_urls(&self) -> Vec<Url> {
    self.rooted_handles().into_iter().map(|(url, _)| url).collect()
  }

  /// Look up one real root without exposing the backing map.
  pub(crate) fn rooted(&self, root: &Url) -> Option<WorkspaceHandle<E>> {
    self.rooted.get(root).cloned()
  }

  /// Insert a real root or return its existing handle.
  fn insert_root(&mut self, root: Url, handle: WorkspaceHandle<E>) -> (WorkspaceHandle<E>, bool) {
    if let Some(existing) = self.rooted.get(&root) {
      return (existing.clone(), false);
    }
    self.rooted.insert(root, handle.clone());
    (handle, true)
  }

  /// Remove one real root without affecting detached state.
  fn remove_root(&mut self, root: &Url) -> Option<WorkspaceHandle<E>> {
    self.rooted.shift_remove(root)
  }
}

/// Whether a real root owns a document URL at a path-segment boundary.
fn root_contains_document(root: &Url, document: &Url) -> bool {
  if root.cannot_be_a_base() || document.cannot_be_a_base() {
    return false;
  }
  if root.scheme() != document.scheme()
    || root.username() != document.username()
    || root.password() != document.password()
    || root.host_str() != document.host_str()
    || root.port_or_known_default() != document.port_or_known_default()
  {
    return false;
  }

  let root_path = root.path().trim_end_matches('/');
  if root_path.is_empty() {
    return document.path().starts_with('/');
  }
  document.path() == root_path
    || document
      .path()
      .strip_prefix(root_path)
      .is_some_and(|remainder| remainder.starts_with('/'))
}

/// Global language-server state.
pub struct WorldState<E: Environment> {
  pub(crate) init_config: ArcSwap<InitConfig>,
  pub(crate) env: E,
  pub(crate) workspaces: AsyncRwLock<Workspaces<E>>,
  pub(crate) default_config: ArcSwap<Config>,
  /// Shared optional remote-schema transport used by every workspace.
  http: Option<reqwest::Client>,
}

impl<E: Environment> WorldState<E> {
  /// Construct a world, degrading only remote schema transport if the HTTP client fails.
  #[must_use]
  pub fn new(env: E) -> Self {
    let http = build_http_client();
    let detached = Arc::new(AsyncRwLock::new(WorkspaceState::new(
      env.clone(),
      WorkspaceRoot::Detached,
      http.clone(),
    )));
    Self {
      init_config: Default::default(),
      workspaces: AsyncRwLock::new(Workspaces::new(detached)),
      default_config: Default::default(),
      env,
      http,
    }
  }

  /// Set the world state's default config.
  pub fn set_default_config(&self, default_config: Arc<Config>) {
    self.default_config.store(default_config);
  }

  /// Clone the current owner handle for one document under a short topology read lock.
  pub(crate) async fn workspace_for_document(&self, document: &Url) -> WorkspaceHandle<E> {
    self.workspaces.read().await.workspace_for_document(document)
  }

  /// Clone one document's immutable handler inputs without retaining workspace locks.
  pub(crate) async fn document_snapshot(&self, document: &Url) -> Option<DocumentSnapshot<E>> {
    let workspace = self.workspace_for_document(document).await;
    workspace.read().await.document_snapshot(document)
  }

  /// Clone all workspace handles under a short topology read lock.
  pub(crate) async fn all_workspace_handles(&self) -> Vec<WorkspaceHandle<E>> {
    self.workspaces.read().await.all_handles()
  }

  /// Clone all real root URLs under a short topology read lock.
  pub(crate) async fn rooted_workspace_urls(&self) -> Vec<Url> {
    self.workspaces.read().await.rooted_urls()
  }

  /// Clone one currently present real-root workspace handle.
  pub(crate) async fn rooted_workspace(&self, root: &Url) -> Option<WorkspaceHandle<E>> {
    self.workspaces.read().await.rooted(root)
  }

  /// Insert a real root and migrate every document whose deepest owner changes to it.
  pub(crate) async fn add_workspace_root(&self, root: Url) -> (WorkspaceHandle<E>, Vec<Url>) {
    let candidate = Arc::new(AsyncRwLock::new(WorkspaceState::new(
      self.env.clone(),
      WorkspaceRoot::Rooted(root.clone()),
      self.http.clone(),
    )));
    let (destination, inserted, sources) = {
      let mut topology = self.workspaces.write().await;
      let (destination, inserted) = topology.insert_root(root, candidate);
      (destination, inserted, topology.all_handles())
    };
    if !inserted {
      return (destination, Vec::new());
    }

    let mut moved = Vec::new();
    for source in sources {
      if Arc::ptr_eq(&source, &destination) {
        continue;
      }
      let document_urls: Vec<Url> = source.read().await.documents.keys().cloned().collect();
      for document_url in document_urls {
        let owner = self.workspace_for_document(&document_url).await;
        if Arc::ptr_eq(&owner, &destination) && move_document(&source, &destination, &document_url).await {
          moved.push(document_url);
        }
      }
    }
    (destination, moved)
  }

  /// Remove a real root and redistribute every open document to its next deepest owner.
  pub(crate) async fn remove_workspace_root(&self, root: &Url) -> Vec<Url> {
    let removed = {
      let mut topology = self.workspaces.write().await;
      topology.remove_root(root)
    };
    let Some(removed) = removed else {
      return Vec::new();
    };

    let document_urls: Vec<Url> = removed.read().await.documents.keys().cloned().collect();
    let mut moved = Vec::new();
    for document_url in document_urls {
      let destination = self.workspace_for_document(&document_url).await;
      if move_document(&removed, &destination, &document_url).await {
        moved.push(document_url);
      }
    }
    moved
  }
}

/// Build the world's sole HTTP client without making world construction fallible.
fn build_http_client() -> Option<reqwest::Client> {
  #[cfg(target_arch = "wasm32")]
  let result = reqwest::Client::builder().build();
  #[cfg(not(target_arch = "wasm32"))]
  let result = taplo_common::util::get_reqwest_client(Duration::from_secs(10));

  match result {
    Ok(client) => Some(client),
    Err(error) => {
      tracing::error!(%error, "remote schema transport is unavailable");
      None
    }
  }
}

/// Move one document and its document-derived association ownership between workspaces.
async fn move_document<E: Environment>(source: &WorkspaceHandle<E>, destination: &WorkspaceHandle<E>, document_url: &Url) -> bool {
  let document = {
    let mut source = source.write().await;
    let document = source.documents.remove(document_url);
    if document.is_some() {
      source.schemas.associations().remove_from_document(document_url);
    }
    document
  };
  let Some(document) = document else {
    return false;
  };

  let mut destination = destination.write().await;
  destination
    .schemas
    .associations()
    .add_from_document(document_url, &document.dom);
  destination.documents.insert(document_url.clone(), document);
  true
}

/// Mutable state owned by one detached or rooted workspace.
pub(crate) struct WorkspaceState<E: Environment> {
  pub(crate) root:         WorkspaceRoot,
  pub(crate) documents:    HashMap<Url, DocumentState>,
  pub(crate) taplo_config: Config,
  pub(crate) schemas:      Schemas<E>,
  pub(crate) config:       LspConfig,
}

impl<E: Environment> WorkspaceState<E> {
  /// Construct one workspace against the world's shared transport capability.
  pub(crate) fn new(env: E, root: WorkspaceRoot, http: Option<reqwest::Client>) -> Self {
    let schemas = match http {
      Some(client) => Schemas::new(env, client),
      None => Schemas::new_offline(env),
    };
    Self {
      root,
      documents: Default::default(),
      taplo_config: Default::default(),
      schemas,
      config: LspConfig::default(),
    }
  }

  /// Clone the document and schema/config inputs needed by async handlers.
  pub(crate) fn document_snapshot(&self, url: &Url) -> Option<DocumentSnapshot<E>> {
    Some(DocumentSnapshot {
      document:     self.documents.get(url)?.clone(),
      schemas:      self.schemas.clone(),
      config:       self.config.clone(),
      taplo_config: self.taplo_config.clone(),
    })
  }

  /// Prepare configuration and schema associations, returning notifications to send unlocked.
  #[tracing::instrument(skip_all, fields(root = ?self.root))]
  pub(crate) async fn initialize(
    &mut self,
    env: &impl Environment,
    default_config: &Config,
  ) -> Result<Vec<DidChangeSchemaAssociationParams>, anyhow::Error> {
    if let Err(error) = self.load_config(env, default_config).await {
      tracing::warn!(%error, "failed to load workspace configuration");
    }

    self.schemas.associations().add_from_config(&self.taplo_config);
    self
      .schemas
      .associations()
      .retain(|(_, association)| association.meta["source"] != source::LSP_CONFIG);

    if !self.config.schema.enabled {
      return Ok(self.association_notifications());
    }

    self.schemas.cache().set_expiration_times(
      Duration::from_secs(self.config.schema.cache.memory_expiration),
      Duration::from_secs(self.config.schema.cache.disk_expiration),
    );

    for (pattern, schema_url) in &self.config.schema.associations {
      let pattern = match Regex::new(pattern) {
        Ok(pattern) => pattern,
        Err(error) => {
          tracing::error!(%error, "invalid association pattern");
          continue;
        }
      };

      let url = if schema_url.starts_with("./") {
        match self.root.url() {
          Some(root) => root.join(schema_url),
          None => {
            tracing::warn!(%schema_url, "relative schema association is unsupported for detached workspace");
            continue;
          }
        }
      } else {
        schema_url.parse()
      };
      let url = match url {
        Ok(url) => url,
        Err(error) => {
          tracing::error!(%error, url = %schema_url, "invalid schema URL");
          continue;
        }
      };
      self
        .schemas
        .associations()
        .add(AssociationRule::Regex(pattern), SchemaAssociation {
          url,
          meta: json!({ "source": source::LSP_CONFIG }),
          priority: priority::LSP_CONFIG,
        });
    }

    for catalog in &self.config.schema.catalogs {
      if let Err(error) = self.schemas.associations().add_from_catalog(catalog).await {
        tracing::error!(%error, "failed to add schemas from catalog");
      }
    }

    Ok(self.association_notifications())
  }

  /// Load and prepare Taplo configuration against the correct domain base.
  pub(crate) async fn load_config(&mut self, env: &impl Environment, default_config: &Config) -> Result<(), anyhow::Error> {
    self.taplo_config = default_config.clone();

    let base_path = match &self.root {
      WorkspaceRoot::Rooted(root) => env.to_file_path_normalized(root).ok_or_else(|| anyhow!("invalid root URL"))?,
      WorkspaceRoot::Detached => env
        .cwd_normalized()
        .ok_or_else(|| anyhow!("current working directory is unavailable for detached workspace"))?,
    };

    if self.config.taplo.config_file.enabled {
      let config_path = match &self.config.taplo.config_file.path {
        Some(path) if env.is_absolute(path) => Some(path.clone()),
        Some(path) if matches!(self.root, WorkspaceRoot::Rooted(_)) => Some(base_path.join(path)),
        Some(path) => {
          tracing::warn!(?path, "relative config path is unsupported for detached workspace");
          None
        }
        None => match self.root {
          WorkspaceRoot::Rooted(_) => env.find_config_file_normalized(&base_path).await,
          WorkspaceRoot::Detached => None,
        },
      };

      if let Some(config_path) = config_path {
        tracing::info!(?config_path, "using config file");
        self.taplo_config = toml::from_str(str::from_utf8(&env.read_file(&config_path).await?)?)?;
      }
    }

    self.taplo_config.rule.extend(self.config.rules.clone());
    self.taplo_config.prepare(env, &base_path)?;
    tracing::debug!(config = ?self.taplo_config, "using workspace config");
    Ok(())
  }

  /// Whether a file document is excluded by a prepared workspace rule.
  pub(crate) fn document_is_excluded(&self, env: &impl Environment, document: &Url) -> bool {
    let Some(path) = env.to_file_path_normalized(document) else {
      return false;
    };
    self.taplo_config.file_rule.as_ref().is_some_and(|rule| !rule.is_match(path))
  }

  /// Build current association notifications without performing client output.
  pub(crate) fn association_notifications(&self) -> Vec<DidChangeSchemaAssociationParams> {
    self
      .documents
      .keys()
      .map(|document_url| {
        let association = self
          .config
          .schema
          .enabled
          .then(|| self.schemas.associations().association_for(document_url))
          .flatten();
        DidChangeSchemaAssociationParams {
          document_uri: document_url.clone(),
          schema_uri:   association.as_ref().map(|value| value.url.clone()),
          meta:         association.map(|value| value.meta),
        }
      })
      .collect()
  }
}

/// Cheap immutable inputs for handlers that may await schema resolution or client output.
#[derive(Clone)]
pub(crate) struct DocumentSnapshot<E: Environment> {
  pub(crate) document:     DocumentState,
  pub(crate) schemas:      Schemas<E>,
  pub(crate) config:       LspConfig,
  pub(crate) taplo_config: Config,
}

/// Parsed open-document state retained across workspace migration.
#[derive(Debug, Clone)]
pub struct DocumentState {
  pub(crate) parse:  Parse,
  pub(crate) dom:    Node,
  pub(crate) mapper: Mapper,
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::sync::Arc;

  use arc_swap::ArcSwap;
  use lsp_async_stub::util::Mapper;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_common::AsyncRwLock;
  use taplo_common::config::Config;
  use taplo_common::schema::associations::source;
  use url::Url;

  use super::DocumentState;
  use super::WorkspaceRoot;
  use super::WorkspaceState;
  use super::Workspaces;
  use super::WorldState;
  use super::root_contains_document;
  use crate::test_support::TestEnvironment;
  use crate::test_support::ensure_anyhow;

  /// Parse one URL fixture through the same public parser used by production configuration.
  fn url(value: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(value), "the workspace fixture URL must parse")
  }

  /// Build a retained document state from one source string.
  fn document(source: &str) -> DocumentState {
    let parse = taplo::parser::parse(source);
    let dom = parse.clone().into_dom();
    DocumentState {
      parse,
      dom,
      mapper: Mapper::new_utf16(source, false),
    }
  }

  /// Construct a world whose schema services are deliberately offline.
  fn offline_world(environment: TestEnvironment) -> WorldState<TestEnvironment> {
    let detached = Arc::new(AsyncRwLock::new(WorkspaceState::new(
      environment.clone(),
      WorkspaceRoot::Detached,
      None,
    )));
    WorldState {
      init_config:    ArcSwap::default(),
      env:            environment,
      workspaces:     AsyncRwLock::new(Workspaces::new(detached)),
      default_config: ArcSwap::default(),
      http:           None,
    }
  }

  #[test]
  fn root_ownership_requires_authority_and_segment_boundaries() -> Result<(), TestFailure> {
    let file_root = url("file:///work/?ignored=true#fragment")?;
    ensure(
      root_contains_document(&file_root, &url("file:///work/file.toml")?),
      "a child path must be owned by its root",
    )?;
    ensure(
      root_contains_document(&file_root, &url("file:///work")?),
      "an exact root path must be owned",
    )?;
    ensure(
      !root_contains_document(&file_root, &url("file:///workspace/file.toml")?),
      "a textual path prefix must not cross a segment boundary",
    )?;

    let web_root = url("https://user@example.com:443/work/")?;
    ensure(
      root_contains_document(&web_root, &url("https://user@example.com/work/a.toml")?),
      "equivalent default ports and authority must match",
    )?;
    ensure(
      !root_contains_document(&web_root, &url("http://user@example.com/work/a.toml")?),
      "a different scheme must not match",
    )?;
    ensure(
      !root_contains_document(&web_root, &url("https://other@example.com/work/a.toml")?),
      "different user information must not match",
    )?;
    ensure(
      !root_contains_document(&web_root, &url("https://elsewhere.test/work/a.toml")?),
      "a different host must not match",
    )?;
    ensure(
      !root_contains_document(&url("mailto:user@example.com")?, &url("mailto:other@example.com")?),
      "opaque URLs must never establish workspace containment",
    )
  }

  #[test]
  fn topology_selects_deepest_root_and_keeps_detached_private() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let detached = Arc::new(AsyncRwLock::new(WorkspaceState::new(
      environment.clone(),
      WorkspaceRoot::Detached,
      None,
    )));
    let parent_url = url("file:///work/")?;
    let nested_url = url("file:///work/nested/")?;
    let parent = Arc::new(AsyncRwLock::new(WorkspaceState::new(
      environment.clone(),
      WorkspaceRoot::Rooted(parent_url.clone()),
      None,
    )));
    let nested = Arc::new(AsyncRwLock::new(WorkspaceState::new(
      environment,
      WorkspaceRoot::Rooted(nested_url.clone()),
      None,
    )));
    let mut topology = Workspaces::new(detached.clone());
    let _ = topology.insert_root(parent_url.clone(), parent.clone());
    let _ = topology.insert_root(nested_url.clone(), nested.clone());

    ensure(
      Arc::ptr_eq(&topology.workspace_for_document(&url("file:///work/nested/file.toml")?), &nested),
      "the deepest matching root must own a nested document",
    )?;
    ensure(
      Arc::ptr_eq(&topology.workspace_for_document(&url("file:///work/parent.toml")?), &parent),
      "the parent root must own its direct child",
    )?;
    ensure(
      Arc::ptr_eq(&topology.workspace_for_document(&url("file:///elsewhere/file.toml")?), &detached),
      "an unrelated document must fall back to detached",
    )?;
    ensure(
      topology.rooted_urls() == vec![parent_url, nested_url],
      "rooted iteration must preserve insertion order without a detached sentinel",
    )?;
    ensure_eq(
      &topology.all_handles().len(),
      &3,
      "all-handle iteration must include exactly one detached workspace",
    )
  }

  #[test]
  fn root_changes_migrate_documents_and_recompute_derived_associations() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let world = offline_world(environment);
    let document_url = url("file:///work/nested/document.toml")?;
    let source = "#:schema https://example.com/schema.json\nvalue = 1\n";

    futures::executor::block_on(async {
      let detached = world.workspace_for_document(&document_url).await;
      {
        let mut workspace = detached.write().await;
        let document = document(source);
        workspace.schemas.associations().add_from_document(&document_url, &document.dom);
        workspace.documents.insert(document_url.clone(), document);
      }

      let parent_url = url("file:///work/")?;
      let (parent, parent_moved) = world.add_workspace_root(parent_url.clone()).await;
      ensure(
        parent_moved == vec![document_url.clone()],
        "adding a parent root must migrate its detached document",
      )?;
      ensure(
        Arc::ptr_eq(&world.workspace_for_document(&document_url).await, &parent),
        "the migrated document must be owned by the parent root",
      )?;

      let nested_url = url("file:///work/nested/")?;
      let (nested, nested_moved) = world.add_workspace_root(nested_url.clone()).await;
      ensure(
        nested_moved == vec![document_url.clone()],
        "adding a deeper root must rebalance a parent-owned document",
      )?;
      let snapshot = ensure_some(
        world.document_snapshot(&document_url).await,
        "the migrated document snapshot must remain available",
      )?;
      let migrated_value = ensure_ok(serde_json::to_value(&snapshot.document.dom), "the migrated DOM must serialize")?;
      ensure_eq(
        &migrated_value,
        &serde_json::json!({ "value": 1 }),
        "migration must preserve the parsed DOM",
      )?;
      let nested_association = ensure_some(
        nested.read().await.schemas.associations().association_for(&document_url),
        "the destination must recompute the document directive",
      )?;
      ensure_eq(
        &nested_association.meta["source"],
        &serde_json::Value::String(source::DIRECTIVE.into()),
        "the migrated association must retain document-derived ownership",
      )?;

      let (duplicate, duplicate_moved) = world.add_workspace_root(nested_url.clone()).await;
      ensure(
        Arc::ptr_eq(&duplicate, &nested),
        "a duplicate root add must reuse the existing workspace",
      )?;
      ensure(duplicate_moved.is_empty(), "a duplicate root add must not rebalance documents")?;

      let removed_nested = world.remove_workspace_root(&nested_url).await;
      ensure(
        removed_nested == vec![document_url.clone()],
        "removing a nested root must migrate its document to the parent",
      )?;
      ensure(
        Arc::ptr_eq(&world.workspace_for_document(&document_url).await, &parent),
        "the surviving parent must regain ownership",
      )?;

      let removed_parent = world.remove_workspace_root(&parent_url).await;
      ensure(
        removed_parent == vec![document_url.clone()],
        "removing the last matching root must migrate to detached",
      )?;
      ensure(
        Arc::ptr_eq(&world.workspace_for_document(&document_url).await, &detached),
        "detached state must survive all root removals",
      )?;

      let (_, unrelated_moved) = world.add_workspace_root(url("file:///unrelated/")?).await;
      ensure(
        unrelated_moved.is_empty(),
        "adding an unrelated root must leave document ownership unchanged",
      )
    })
  }

  #[test]
  fn workspace_configuration_uses_domain_specific_bases_and_safe_fallbacks() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    environment.insert_file("/workspace/taplo.toml", b"include = [\"**/*.discovered\"]\n".to_vec());
    environment.insert_file("/configs/absolute.toml", b"include = [\"**/*.absolute\"]\n".to_vec());
    let default_config = Config::default();

    futures::executor::block_on(async {
      let mut detached = WorkspaceState::new(environment.clone(), WorkspaceRoot::Detached, None);
      ensure_anyhow(
        detached.load_config(&environment, &default_config).await,
        "detached default configuration must prepare from the CWD",
      )?;
      ensure(
        environment.discovery_bases().is_empty(),
        "detached configuration must not auto-discover a root config file",
      )?;

      detached.config.taplo.config_file.path = Some(PathBuf::from("/configs/absolute.toml"));
      ensure_anyhow(
        detached.load_config(&environment, &default_config).await,
        "an absolute detached config path must load",
      )?;
      ensure(
        detached
          .taplo_config
          .is_included(PathBuf::from("/workspace/file.absolute").as_path()),
        "the explicit absolute config must replace detached defaults",
      )?;

      detached.config.taplo.config_file.path = Some(PathBuf::from("relative.toml"));
      ensure_anyhow(
        detached.load_config(&environment, &default_config).await,
        "an unsupported relative detached path must fall back to prepared defaults",
      )?;
      ensure(
        detached.taplo_config.file_rule.is_some(),
        "skipping a relative detached path must still prepare the active config",
      )?;

      let rooted_url = url("file:///workspace/project/")?;
      environment.insert_file("/workspace/project/taplo.toml", b"include = [\"**/*.rooted\"]\n".to_vec());
      let mut rooted = WorkspaceState::new(environment.clone(), WorkspaceRoot::Rooted(rooted_url), None);
      ensure_anyhow(
        rooted.load_config(&environment, &default_config).await,
        "rooted configuration must discover from its normalized root",
      )?;
      ensure(
        rooted
          .taplo_config
          .is_included(PathBuf::from("/workspace/project/file.rooted").as_path()),
        "root discovery must load the root-local configuration",
      )?;
      ensure(
        environment.discovery_bases().contains(&PathBuf::from("/workspace/project")),
        "root discovery must receive the normalized root path",
      )?;

      let no_cwd_environment = TestEnvironment::default();
      no_cwd_environment.set_cwd(None);
      let mut no_cwd = WorkspaceState::new(no_cwd_environment.clone(), WorkspaceRoot::Detached, None);
      let result = no_cwd.load_config(&no_cwd_environment, &default_config).await;
      ensure(
        result.is_err(),
        "detached configuration without a CWD must return a typed preparation error",
      )?;
      ensure(
        !no_cwd.document_is_excluded(&no_cwd_environment, &url("file:///workspace/file.toml")?),
        "an unprepared detached rule must not exclude every file",
      )
    })
  }

  #[test]
  fn offline_workspaces_keep_local_schemas_and_root_relative_associations() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let default_config = Config::default();

    futures::executor::block_on(async {
      let root_url = url("file:///workspace/project/")?;
      let document_url = url("file:///workspace/project/file.toml")?;
      let mut rooted = WorkspaceState::new(environment.clone(), WorkspaceRoot::Rooted(root_url), None);
      rooted.config.schema.catalogs.clear();
      rooted
        .config
        .schema
        .associations
        .insert(".*\\.toml$".into(), "./schema.json".into());
      ensure_anyhow(
        rooted.initialize(&environment, &default_config).await,
        "the rooted offline workspace must initialize",
      )?;
      let association = ensure_some(
        rooted.schemas.associations().association_for(&document_url),
        "a rooted relative schema association must be installed",
      )?;
      ensure_eq(
        &association.url,
        &url("file:///workspace/project/schema.json")?,
        "root-relative schema URLs must resolve against the real root",
      )?;

      let builtin = ensure_anyhow(
        rooted
          .schemas
          .load_schema(&url(taplo_common::schema::builtins::TAPLO_CONFIG_URL)?)
          .await,
        "offline schema services must retain built-in resolution",
      )?;
      ensure(builtin.is_object(), "the offline built-in schema must remain usable")?;

      let mut detached = WorkspaceState::new(environment.clone(), WorkspaceRoot::Detached, None);
      detached.config.schema.catalogs.clear();
      detached
        .config
        .schema
        .associations
        .insert(".*\\.toml$".into(), "./schema.json".into());
      ensure_anyhow(
        detached.initialize(&environment, &default_config).await,
        "the detached workspace must initialize while skipping relative schema URLs",
      )?;
      ensure(
        detached
          .schemas
          .associations()
          .read()
          .iter()
          .all(|(_, association)| association.meta["source"] != source::LSP_CONFIG),
        "detached state must not manufacture a root for relative schema associations",
      )
    })
  }
}
