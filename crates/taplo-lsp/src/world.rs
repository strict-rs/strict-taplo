//! Revisioned language-server state shared by local and concurrent runtimes.

use std::collections::hash_map::Entry;
use std::fmt;
use std::future::Future;
use std::iter::once;
#[cfg(test)]
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::str;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use parking_lot::RwLock;
use regex::Regex;
use serde_json::Value;
use serde_json::json;
use taplo::dom::Keys;
use taplo::dom::Node;
use taplo::parser::Parse;
use taplo::parser::ParseFailure;
use taplo::parser::parse;
use taplo_common::AsyncRwLock;
use taplo_common::HashMap;
use taplo_common::IndexMap;
#[cfg(test)]
use taplo_common::config::CONFIG_FILE_NAMES;
use taplo_common::config::Config;
use taplo_common::config::ConfigError;
#[cfg(not(target_arch = "wasm32"))]
use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::Environment;
use taplo_common::environment::EnvironmentError;
use taplo_common::environment::LocalEnvironment;
use taplo_common::schema::NodeValidationError;
use taplo_common::schema::SchemaError;
use taplo_common::schema::Schemas;
use taplo_common::schema::associations::AssociationError;
use taplo_common::schema::associations::AssociationRule;
use taplo_common::schema::associations::SchemaAssociation;
use taplo_common::schema::associations::SchemaAssociations;
use taplo_common::schema::associations::priority;
use taplo_common::schema::associations::source;
use taplo_common::schema::cache::CacheError;
#[cfg(not(target_arch = "wasm32"))]
use taplo_common::schema::transport::ConcurrentSchemaTransport;
#[cfg(not(target_arch = "wasm32"))]
use taplo_common::schema::transport::ConcurrentTransport;
use taplo_common::schema::transport::LocalSchemaTransport;
use taplo_common::schema::transport::SchemaTransport;
use taplo_common::util::GlobRuleError;
use taplo_lsp_async::util::Mapper;
use taplo_lsp_async::util::MappingError;
#[cfg(test)]
use taplo_test_support::TestEnvironment as SharedTestEnvironment;
use thiserror::Error;
#[cfg(test)]
use time::OffsetDateTime;
use toml::de::Error as TomlError;
use url::ParseError as UrlParseError;
use url::Url;

use crate::config::InitConfig;
use crate::config::LspConfig;
use crate::config::LspConfigError;
use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;

/// Shared local world ownership used by the WebAssembly server.
pub type LocalWorld<E> = Rc<WorldState<E, LocalSchemaTransport<E>>>;

/// Shared concurrent world ownership used by the native server.
#[cfg(not(target_arch = "wasm32"))]
pub type ConcurrentWorld<E> = Arc<WorldState<E, ConcurrentSchemaTransport<E>>>;

/// Schema-validation result consumed by diagnostics.
type ValidationResult = Result<Vec<NodeValidationError>, SchemaError>;

/// Schema candidates associated with one exact semantic path.
type PathSchemasResult = Result<Vec<(Keys, Arc<Value>)>, SchemaError>;

/// Schema candidates reachable beneath one completion path.
type CompletionSchemasResult = Result<Vec<(Keys, Keys, Arc<Value>)>, SchemaError>;

/// Catalog-replacement result consumed by workspace configuration.
type CatalogResult = Result<(), AssociationError>;

/// Select local-capable or concurrent schema operations without duplicating semantic behavior.
pub(crate) trait SchemaExecution<T: SchemaTransport> {
  /// Future returned by [`Self::validate_root`].
  type ValidationFuture<'schema>: Future<Output = ValidationResult> + 'schema
  where
    T: 'schema;

  /// Future returned by [`Self::schemas_at_path`].
  type PathSchemasFuture<'schema>: Future<Output = PathSchemasResult> + 'schema
  where
    T: 'schema;

  /// Future returned by [`Self::possible_schemas_from`].
  type CompletionSchemasFuture<'schema>: Future<Output = CompletionSchemasResult> + 'schema
  where
    T: 'schema;

  /// Future returned by [`Self::replace_catalogs`].
  type CatalogFuture<'schema>: Future<Output = CatalogResult> + 'schema
  where
    T: 'schema;

  /// Validate one DOM root through the selected operation family.
  fn validate_root<'schema>(schemas: &'schema Schemas<T>, schema_url: &'schema Url, root: &'schema Node)
  -> Self::ValidationFuture<'schema>;

  /// Resolve schema candidates at one semantic path.
  fn schemas_at_path<'schema>(
    schemas: &'schema Schemas<T>,
    schema_url: &'schema Url,
    instance: &'schema Value,
    path: &'schema Keys,
  ) -> Self::PathSchemasFuture<'schema>;

  /// Resolve completion candidates beneath one semantic path.
  fn possible_schemas_from<'schema>(
    schemas: &'schema Schemas<T>,
    schema_url: &'schema Url,
    instance: &'schema Value,
    path: &'schema Keys,
    depth: usize,
  ) -> Self::CompletionSchemasFuture<'schema>;

  /// Replace catalog-owned associations through the selected operation family.
  fn replace_catalogs<'schema>(associations: &'schema SchemaAssociations<T>, catalogs: &'schema [Url]) -> Self::CatalogFuture<'schema>;
}

/// Implement the schema-operation selector for one executor capability family.
macro_rules! implement_schema_execution {
  (
    execution = $execution:ident,
    future_bounds = { $($future_bound:tt)* },
    validate_root = $validate_root:ident,
    schemas_at_path = $schemas_at_path:ident,
    possible_schemas_from = $possible_schemas_from:ident,
    replace_catalogs = $replace_catalogs:ident,
    bounds = { $($bounds:tt)* }
  ) => {
    impl<T> SchemaExecution<T> for $execution
    where
      $($bounds)*
    {
      type ValidationFuture<'schema>
        = Pin<Box<dyn Future<Output = ValidationResult> $($future_bound)* + 'schema>>
      where
        T: 'schema;
      type PathSchemasFuture<'schema>
        = Pin<Box<dyn Future<Output = PathSchemasResult> $($future_bound)* + 'schema>>
      where
        T: 'schema;
      type CompletionSchemasFuture<'schema>
        = Pin<Box<dyn Future<Output = CompletionSchemasResult> $($future_bound)* + 'schema>>
      where
        T: 'schema;
      type CatalogFuture<'schema>
        = Pin<Box<dyn Future<Output = CatalogResult> $($future_bound)* + 'schema>>
      where
        T: 'schema;

      fn validate_root<'schema>(
        schemas: &'schema Schemas<T>,
        schema_url: &'schema Url,
        root: &'schema Node,
      ) -> Self::ValidationFuture<'schema> {
        schemas.$validate_root(schema_url, root)
      }

      fn schemas_at_path<'schema>(
        schemas: &'schema Schemas<T>,
        schema_url: &'schema Url,
        instance: &'schema Value,
        path: &'schema Keys,
      ) -> Self::PathSchemasFuture<'schema> {
        schemas.$schemas_at_path(schema_url, instance, path)
      }

      fn possible_schemas_from<'schema>(
        schemas: &'schema Schemas<T>,
        schema_url: &'schema Url,
        instance: &'schema Value,
        path: &'schema Keys,
        depth: usize,
      ) -> Self::CompletionSchemasFuture<'schema> {
        schemas.$possible_schemas_from(schema_url, instance, path, depth)
      }

      fn replace_catalogs<'schema>(
        associations: &'schema SchemaAssociations<T>,
        catalogs: &'schema [Url],
      ) -> Self::CatalogFuture<'schema> {
        associations.$replace_catalogs(catalogs)
      }
    }
  };
}

/// Select schema operations whose futures may remain on the current thread.
pub(crate) enum LocalSchemaExecution {}

implement_schema_execution!(
  execution = LocalSchemaExecution,
  future_bounds = {},
  validate_root = validate_root,
  schemas_at_path = schemas_at_path,
  possible_schemas_from = possible_schemas_from,
  replace_catalogs = replace_catalogs,
  bounds = { T: SchemaTransport }
);

/// Select schema operations whose futures may cross thread boundaries.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum ConcurrentSchemaExecution {}

#[cfg(not(target_arch = "wasm32"))]
implement_schema_execution!(
  execution = ConcurrentSchemaExecution,
  future_bounds = { + Send },
  validate_root = validate_root_concurrent,
  schemas_at_path = schemas_at_path_concurrent,
  possible_schemas_from = possible_schemas_from_concurrent,
  replace_catalogs = replace_catalogs_concurrent,
  bounds = {
    T: ConcurrentTransport,
    for<'transport> T::ReadBytesFuture<'transport>: Send,
    for<'transport> T::ReadFuture<'transport>: Send,
    for<'transport> T::WriteFuture<'transport>: Send
  }
);

/// Test-target adapter from the shared deterministic host into Taplo's environment traits.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct TestEnvironment(SharedTestEnvironment);

#[cfg(test)]
taplo_test_support::implement_local_test_environment!(
  TestEnvironment,
  CONFIG_FILE_NAMES,
  taplo_common::implement_file_path_environment,
  taplo_common::implement_local_environment
);
#[cfg(test)]
taplo_test_support::implement_concurrent_test_environment!(TestEnvironment, CONFIG_FILE_NAMES);

/// A typed world-state construction, mutation, or snapshot failure.
#[derive(Debug, Error)]
pub enum WorldError {
  /// Syntax-tree construction failed.
  #[error(transparent)]
  Parse(#[from] ParseFailure),
  /// Source-to-LSP coordinate construction failed.
  #[error(transparent)]
  Mapping(#[from] MappingError),
  /// Schema service initialization or interpretation failed.
  #[error(transparent)]
  Schema(#[from] SchemaError),
  /// Schema cache policy or persistence failed.
  #[error(transparent)]
  Cache(#[from] CacheError),
  /// Schema association preparation failed.
  #[error(transparent)]
  Association(#[from] AssociationError),
  /// A host capability failed.
  #[error(transparent)]
  Environment(#[from] EnvironmentError),
  /// Taplo configuration preparation failed.
  #[error(transparent)]
  Config(#[from] ConfigError),
  /// LSP configuration construction or decoding failed.
  #[error(transparent)]
  LspConfig(#[from] LspConfigError),
  /// A configuration file was not valid UTF-8.
  #[error("configuration file `{path}` is not valid UTF-8")]
  ConfigUtf8 {
    /// Configuration file path.
    path:   PathBuf,
    /// Underlying UTF-8 failure.
    #[source]
    source: str::Utf8Error,
  },
  /// A configuration file was not valid TOML.
  #[error("configuration file `{path}` is not valid TOML")]
  ConfigToml {
    /// Configuration file path.
    path:   PathBuf,
    /// Underlying TOML failure.
    #[source]
    source: TomlError,
  },
  /// A rooted workspace URL cannot be represented in the host path model.
  #[error("workspace root `{root}` is not a valid host file path")]
  InvalidWorkspaceRoot {
    /// Rejected workspace root.
    root: Url,
  },
  /// The detached workspace has no current working directory.
  #[error("current working directory is unavailable for the detached workspace")]
  MissingCurrentDirectory,
  /// A detached workspace cannot resolve a relative configured path.
  #[error("relative path `{path}` is unsupported for the detached workspace")]
  DetachedRelativePath {
    /// Rejected relative path.
    path: PathBuf,
  },
  /// An LSP association regular expression is invalid.
  #[error("invalid LSP schema association pattern `{pattern}`")]
  AssociationPattern {
    /// Rejected expression.
    pattern: String,
    /// Underlying regex failure.
    #[source]
    source:  regex::Error,
  },
  /// A manual schema-association glob is invalid.
  #[error(transparent)]
  AssociationGlob(#[from] GlobRuleError),
  /// A manual schema-association regular expression is invalid.
  #[error("invalid manual schema association pattern `{pattern}`")]
  ManualAssociationPattern {
    /// Rejected expression.
    pattern: String,
    /// Underlying regex failure.
    #[source]
    source:  regex::Error,
  },
  /// An LSP association URL is invalid.
  #[error("invalid LSP schema association URL `{source_value}`")]
  AssociationUrl {
    /// Rejected URL source.
    source_value: String,
    /// Underlying URL failure.
    #[source]
    source:       UrlParseError,
  },
  /// The monotonically increasing world revision is exhausted.
  #[error("world revision space is exhausted")]
  RevisionExhausted,
  /// A configuration response value is missing or not an object.
  #[error("invalid workspace configuration value for {scope}: {reason}")]
  ConfigurationResponse {
    /// Global or rooted scope description.
    scope:  String,
    /// Stable shape failure.
    reason: &'static str,
  },
  /// A configuration response refers to a workspace that is no longer present.
  #[error("workspace `{root}` is no longer present")]
  MissingWorkspace {
    /// Missing workspace root.
    root: Url,
  },
  /// Initialization attempted to install a second workspace topology.
  #[error("language-server workspace topology is already initialized")]
  AlreadyInitialized,
  /// Initialization supplied the same workspace root more than once.
  #[error("workspace root `{root}` was supplied more than once")]
  DuplicateWorkspaceRoot {
    /// Duplicate workspace root.
    root: Url,
  },
  /// A document appears in more than one workspace seed.
  #[error("document `{document}` is owned by more than one workspace")]
  DuplicateDocumentOwnership {
    /// Document with ambiguous ownership.
    document: Url,
  },
}

/// A checked state revision used for documents, configuration, schemas, and diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Revision(u64);

impl Revision {
  /// Initial state before the first committed mutation.
  const INITIAL: Self = Self(0);
}

/// Per-workspace lock handle; topology and workspace state have separate ownership.
pub(crate) type WorkspaceHandle<T> = Arc<AsyncRwLock<WorkspaceState<T>>>;

/// Domain identity of a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceRoot {
  /// Permanent workspace for documents outside every configured root.
  Detached,
  /// Workspace owned by a real root URL.
  Rooted(Url),
}

impl WorkspaceRoot {
  /// Return the real root URL, if this is a rooted workspace.
  const fn url(&self) -> Option<&Url> {
    match *self {
      Self::Detached => None,
      Self::Rooted(ref url) => Some(url),
    }
  }
}

/// Workspace topology with an explicit detached state and insertion-ordered real roots.
pub(crate) struct Workspaces<T: SchemaTransport> {
  /// Always-present detached workspace.
  detached: WorkspaceHandle<T>,
  /// Real rooted workspaces.
  rooted:   IndexMap<Url, WorkspaceHandle<T>>,
}

impl<T: SchemaTransport> Workspaces<T> {
  /// Construct topology around the permanent detached workspace.
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor establishes the topology invariant that the detached workspace is always present and real roots start \
              empty, which every ownership lookup here depends on"
  )]
  fn new(detached: WorkspaceHandle<T>) -> Self {
    Self {
      detached,
      rooted: IndexMap::default(),
    }
  }

  /// Select the deepest real root containing `document`, otherwise detached.
  fn workspace_for_document(&self, document: &Url) -> WorkspaceHandle<T> {
    self
      .rooted
      .iter()
      .filter(|entry| root_contains_document(entry.0, document))
      .max_by_key(|entry| entry.0.path().len())
      .map_or_else(|| Arc::clone(&self.detached), |entry| Arc::clone(entry.1))
  }

  /// Return detached plus every rooted workspace handle.
  fn all_handles(&self) -> Vec<WorkspaceHandle<T>> {
    once(Arc::clone(&self.detached))
      .chain(self.rooted.values().map(Arc::clone))
      .collect()
  }

  /// Return every real root URL in insertion order.
  fn rooted_urls(&self) -> Vec<Url> {
    self.rooted.keys().cloned().collect()
  }

  /// Look up one real root.
  fn rooted(&self, root: &Url) -> Option<WorkspaceHandle<T>> {
    self.rooted.get(root).cloned()
  }
}

/// One rooted workspace in a fully prepared topology replacement.
struct PlannedRoot {
  /// Root identity retained in insertion order.
  root:           Url,
  /// Semantic inputs assigned to this root.
  seed:           WorkspaceSeed,
  /// Whether this root must discover and load host configuration.
  load_from_host: bool,
}

/// Complete semantic topology replacement prepared before creating schema services.
struct TopologyPlan {
  /// Permanent detached workspace inputs.
  detached:  WorkspaceSeed,
  /// Desired real roots in insertion order.
  rooted:    Vec<PlannedRoot>,
  /// Every open document affected by ownership or configuration recomputation.
  documents: Vec<Url>,
}

/// Inputs collected once before documents and exact associations are redistributed.
struct RedistributionInputs {
  /// Non-URL manual rules copied into every workspace.
  global_manual: Vec<(AssociationRule, SchemaAssociation)>,
  /// URL-specific manual rules assigned to their deepest owner.
  exact_manual:  Vec<(AssociationRule, SchemaAssociation)>,
  /// Uniquely owned open documents awaiting reassignment.
  documents:     HashMap<Url, DocumentState>,
}

/// Compute the requested rooted topology and whether each root needs host loading.
fn desired_roots(existing: &IndexMap<Url, WorkspaceSeed>, removed: &[Url], added: &[Url]) -> Option<Vec<(Url, bool)>> {
  let mut desired = Vec::new();
  for root in existing.keys() {
    if !removed.contains(root) {
      desired.push((root.clone(), false));
    }
  }
  for root in added {
    if !desired.iter().any(|desired_entry| &desired_entry.0 == root) {
      desired.push((root.clone(), true));
    }
  }
  let topology_changed = desired.len() != existing.len()
    || desired
      .iter()
      .any(|desired_entry| desired_entry.1 || !existing.contains_key(&desired_entry.0));
  topology_changed.then_some(desired)
}

/// Collect unique documents and partition global versus URL-specific manual associations.
///
/// # Errors
///
/// Returns [`WorldError::DuplicateDocumentOwnership`] when existing workspace seeds already claim
/// the same document.
fn redistribution_inputs(detached: &WorkspaceSeed, existing: &IndexMap<Url, WorkspaceSeed>) -> Result<RedistributionInputs, WorldError> {
  let global_manual = detached
    .manual_associations
    .iter()
    .filter(|association_entry| !matches!(association_entry.0, AssociationRule::Url(_)))
    .cloned()
    .collect();
  let mut exact_manual = detached
    .manual_associations
    .iter()
    .filter(|association_entry| matches!(association_entry.0, AssociationRule::Url(_)))
    .cloned()
    .collect::<Vec<_>>();
  let mut documents = HashMap::default();
  merge_unique_documents(&mut documents, detached.documents.clone())?;
  for seed in existing.values() {
    merge_unique_documents(&mut documents, seed.documents.clone())?;
    exact_manual.extend(
      seed
        .manual_associations
        .iter()
        .filter(|association_entry| matches!(association_entry.0, AssociationRule::Url(_)))
        .cloned(),
    );
  }
  Ok(RedistributionInputs {
    global_manual,
    exact_manual,
    documents,
  })
}

/// Effects of one committed topology replacement.
#[derive(Debug)]
pub(crate) struct TopologyUpdate {
  /// Current schema-association notifications for every open document.
  pub(crate) notifications: Vec<DidChangeSchemaAssociationParams>,
  /// Every open document requiring a diagnostics refresh.
  pub(crate) documents:     Vec<Url>,
}

/// Complete topology state prepared before publication.
struct PreparedTopology<T: SchemaTransport> {
  /// Replacement topology.
  workspaces:    Workspaces<T>,
  /// Current schema-association notifications.
  notifications: Vec<DidChangeSchemaAssociationParams>,
  /// Documents affected by the ownership change.
  documents:     Vec<Url>,
}

impl<T: SchemaTransport> PreparedTopology<T> {
  /// Build owned topology handles and collect effects from validated unique roots.
  fn new(detached: WorkspaceState<T>, rooted_states: Vec<(Url, WorkspaceState<T>)>, documents: Vec<Url>) -> Self {
    let mut notifications = detached.association_notifications();
    for state_entry in &rooted_states {
      notifications.extend(state_entry.1.association_notifications());
    }
    let rooted_handles = rooted_states
      .into_iter()
      .map(|(root, state)| (root, Arc::new(AsyncRwLock::new(state))))
      .collect();
    Self {
      workspaces: Workspaces {
        detached: Arc::new(AsyncRwLock::new(detached)),
        rooted:   rooted_handles,
      },
      notifications,
      documents,
    }
  }
}

/// Return whether a real root owns a document URL at a path-segment boundary.
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

/// Select the deepest root containing one document.
fn deepest_root<'root>(roots: impl Iterator<Item = &'root Url>, document: &Url) -> Option<&'root Url> {
  roots
    .filter(|root| root_contains_document(root, document))
    .max_by_key(|root| root.path().len())
}

/// Reject duplicate workspace roots before any initialization work begins.
fn ensure_unique_roots(roots: &[Url]) -> Result<(), WorldError> {
  let mut seen = Vec::with_capacity(roots.len());
  for root in roots {
    if seen.contains(root) {
      return Err(WorldError::DuplicateWorkspaceRoot {
        root: root.clone()
      });
    }
    seen.push(root.clone());
  }
  Ok(())
}

/// Global language-server state parameterized by an execution-model-specific schema transport.
pub struct WorldState<E: Environment, T: SchemaTransport> {
  /// Initialization configuration supplied by the client.
  init_config:    RwLock<Arc<InitConfig>>,
  /// Host capabilities.
  pub(crate) env: E,
  /// Workspace topology and mutable workspace state.
  workspaces:     AsyncRwLock<Workspaces<T>>,
  /// Default Taplo configuration supplied by the CLI or embedding host.
  default_config: RwLock<Arc<Config>>,
  /// Transport cloned into newly added workspaces.
  transport:      T,
  /// Short global barrier preventing snapshots from observing partial mutations.
  state_barrier:  AsyncRwLock<()>,
  /// Next checked committed-state revision.
  revision:       AtomicU64,
}

impl<E: Environment, T: SchemaTransport> fmt::Debug for WorldState<E, T> {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut state = formatter.debug_struct("WorldState");
    let mut fields = state.field("revision", &self.revision.load(Ordering::SeqCst));
    if let Ok(workspaces) = self.workspaces.try_read() {
      fields = fields.field("rooted_workspaces", &workspaces.rooted.len());
      let document_count = workspaces.all_handles().into_iter().try_fold(0_usize, |count, handle| {
        let workspace = handle.try_read().ok()?;
        count.checked_add(workspace.documents.len())
      });
      if let Some(resolved_document_count) = document_count {
        fields = fields.field("open_documents", &resolved_document_count);
      }
    }
    fields.finish_non_exhaustive()
  }
}

impl<E: Environment, T: SchemaTransport> WorldState<E, T> {
  /// Construct a world around an explicit schema transport.
  ///
  /// # Errors
  ///
  /// Returns [`WorldError`] when the detached workspace's schema services cannot initialize.
  pub fn with_transport(env: E, transport: T) -> Result<Self, WorldError> {
    let detached = Arc::new(AsyncRwLock::new(WorkspaceState::new(WorkspaceRoot::Detached, transport.clone())?));
    Ok(Self {
      init_config: RwLock::new(Arc::new(InitConfig::default())),
      env,
      workspaces: AsyncRwLock::new(Workspaces::new(detached)),
      default_config: RwLock::new(Arc::new(Config::default())),
      transport,
      state_barrier: AsyncRwLock::new(()),
      revision: AtomicU64::new(Revision::INITIAL.0),
    })
  }

  /// Replace the default Taplo configuration.
  pub fn set_default_config(&self, default_config: Arc<Config>) {
    *self.default_config.write() = default_config;
  }

  /// Set the embedding host's default schema-cache path.
  ///
  /// A client-provided initialization path takes precedence. This default is
  /// retained when the client omits `cachePath`.
  pub fn set_default_cache_path(&self, cache_path: Option<PathBuf>) {
    let current = self.init_config();
    *self.init_config.write() = Arc::new(InitConfig {
      cache_path,
      configuration_section: current.configuration_section.clone(),
    });
  }

  /// Clone the default Taplo configuration.
  pub(crate) fn default_config(&self) -> Arc<Config> {
    self.default_config.read().clone()
  }

  /// Clone initialization configuration.
  pub(crate) fn init_config(&self) -> Arc<InitConfig> {
    self.init_config.read().clone()
  }

  /// Merge client initialization with embedding-host defaults.
  fn effective_init_config(&self, init_config: Arc<InitConfig>) -> Arc<InitConfig> {
    if init_config.cache_path.is_some() {
      return init_config;
    }
    let default = self.init_config();
    Arc::new(InitConfig {
      cache_path:            default.cache_path.clone(),
      configuration_section: init_config.configuration_section.clone(),
    })
  }

  /// Calculate the next revision without publishing it.
  fn candidate_revision(&self) -> Result<Revision, WorldError> {
    self
      .revision
      .load(Ordering::SeqCst)
      .checked_add(1)
      .map(Revision)
      .ok_or(WorldError::RevisionExhausted)
  }

  /// Publish a fully committed revision.
  fn commit_revision(&self, revision: Revision) {
    self.revision.store(revision.0, Ordering::SeqCst);
  }
}

/// Generate world operations whose semantics are identical across execution families.
macro_rules! define_world_state_future_family {
  (
    future = $future:ident,
    environment_bounds = [$($environment_bound:path),*],
    transport_bounds = [$($transport_bound:path),*],
    operation_bounds = [$($operation_bound:path),*],
    with_document_workspace = $with_document_workspace:ident,
    document_snapshot = $document_snapshot:ident,
    snapshot_is_current = $snapshot_is_current:ident,
    rooted_workspace_urls = $rooted_workspace_urls:ident,
    plan_topology_change = $plan_topology_change:ident,
    replace_document = $replace_document:ident,
    close_document = $close_document:ident,
    open_document_dispositions = $open_document_dispositions:ident,
    list_schema_associations = $list_schema_associations:ident,
    associated_schema = $associated_schema:ident,
    associate_schema = $associate_schema:ident,
  ) => {
    impl<E, T> WorldState<E, T>
    where
      E: Environment $(+ $environment_bound)*,
      T: SchemaTransport $(+ $transport_bound)*,
    {
      /// Execute one read operation against the workspace selected for a document.
      fn $with_document_workspace<'operation, R, Operation, OperationFuture>(
        &'operation self,
        document: &'operation Url,
        operation: Operation,
      ) -> $future<'operation, R>
      where
        R: 'operation $(+ $operation_bound)*,
        Operation: FnOnce(WorkspaceHandle<T>) -> OperationFuture + 'operation $(+ $operation_bound)*,
        OperationFuture: Future<Output = R> + 'operation $(+ $operation_bound)*,
      {
        Box::pin(async move {
          let _barrier = self.state_barrier.read().await;
          let workspace = self.workspaces.read().await.workspace_for_document(document);
          operation(workspace).await
        })
      }

      /// Clone one document's immutable handler inputs without retaining workspace locks.
      pub(crate) fn $document_snapshot<'operation>(
        &'operation self,
        document: &'operation Url,
      ) -> $future<'operation, Option<DocumentSnapshot<T>>> {
        Box::pin(async move {
          let _barrier = self.state_barrier.read().await;
          let workspace = self.workspaces.read().await.workspace_for_document(document);
          workspace.read().await.document_snapshot(document)
        })
      }

      /// Return whether a captured document snapshot is still current.
      pub(crate) fn $snapshot_is_current<'operation>(
        &'operation self,
        document: &'operation Url,
        snapshot: &'operation DocumentSnapshot<T>,
      ) -> $future<'operation, bool> {
        Box::pin(async move {
          let Some(current) = self.$document_snapshot(document).await else {
            return false;
          };
          current.document.revision == snapshot.document.revision
            && current.config_revision == snapshot.config_revision
            && current.schema_revision == snapshot.schema_revision
        })
      }

      /// Clone all real root URLs.
      pub(crate) fn $rooted_workspace_urls(&self) -> $future<'_, Vec<Url>> {
        Box::pin(async move {
          let _barrier = self.state_barrier.read().await;
          self.workspaces.read().await.rooted_urls()
        })
      }

      /// Capture and redistribute semantic workspace inputs without mutating live topology.
      fn $plan_topology_change<'operation>(
        &'operation self,
        removed: &'operation [Url],
        added: &'operation [Url],
      ) -> $future<'operation, Result<Option<TopologyPlan>, WorldError>> {
        Box::pin(async move {
          let topology = self.workspaces.read().await;
          let mut detached = topology.detached.read().await.seed();
          let global_config = detached.config.clone();
          let mut existing_seeds = Vec::with_capacity(topology.rooted.len());
          for rooted_entry in &topology.rooted {
            existing_seeds.push((rooted_entry.0.clone(), rooted_entry.1.read().await.seed()));
          }
          drop(topology);
          let mut existing: IndexMap<_, _> = existing_seeds.into_iter().collect();

          let Some(desired) = desired_roots(&existing, removed, added) else {
            return Ok(None);
          };
          let redistribution = redistribution_inputs(&detached, &existing)?;

          detached.documents.clear();
          detached.manual_associations.clone_from(&redistribution.global_manual);
          let mut rooted = Vec::with_capacity(desired.len());
          for (root, load_from_host) in desired {
            let mut seed = if load_from_host {
              let mut added = WorkspaceSeed::empty(WorkspaceRoot::Rooted(root.clone()))?;
              added.config.clone_from(&global_config);
              added
            } else {
              existing.shift_remove(&root).ok_or_else(|| WorldError::MissingWorkspace {
                root: root.clone()
              })?
            };
            seed.documents.clear();
            seed.manual_associations.clone_from(&redistribution.global_manual);
            rooted.push(PlannedRoot {
              root,
              seed,
              load_from_host,
            });
          }

          let mut redistributed_documents: Vec<_> = redistribution.documents.into_iter().collect();
          redistributed_documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
          for (document_url, document) in redistributed_documents {
            let Some(owner) = deepest_root(rooted.iter().map(|planned| &planned.root), &document_url).cloned() else {
              insert_unique_document(&mut detached.documents, document_url, document)?;
              continue;
            };
            let Some(planned) = rooted.iter_mut().find(|planned| planned.root == owner) else {
              return Err(WorldError::MissingWorkspace {
                root: owner
              });
            };
            insert_unique_document(&mut planned.seed.documents, document_url, document)?;
          }

          for (rule, association) in redistribution.exact_manual {
            let document_url = match rule {
              AssociationRule::Url(ref document_url) => document_url,
              AssociationRule::Glob(_) | AssociationRule::Regex(_) => continue,
            };
            let Some(owner) = deepest_root(rooted.iter().map(|planned| &planned.root), document_url).cloned() else {
              detached.manual_associations.push((rule, association));
              continue;
            };
            let Some(planned) = rooted.iter_mut().find(|planned| planned.root == owner) else {
              return Err(WorldError::MissingWorkspace {
                root: owner
              });
            };
            planned.seed.manual_associations.push((rule, association));
          }

          let mut documents: Vec<_> = detached.documents.keys().cloned().collect();
          for planned in &rooted {
            documents.extend(planned.seed.documents.keys().cloned());
          }
          documents.sort_by(|left, right| left.as_str().cmp(right.as_str()));
          documents.dedup();
          Ok(Some(TopologyPlan {
            detached,
            rooted,
            documents,
          }))
        })
      }

      /// Parse and atomically install one open document.
      ///
      /// # Errors
      ///
      /// Returns [`WorldError`] when parsing, mapping, path conversion, or association preparation
      /// fails.
      pub(crate) fn $replace_document<'operation>(
        &'operation self,
        document_url: &'operation Url,
        source_text: &'operation str,
      ) -> $future<'operation, Result<DocumentUpdate, WorldError>> {
        Box::pin(async move {
          let _barrier_guard = self.state_barrier.write().await;
          let revision = self.candidate_revision()?;
          let mut document = DocumentState::parse_at(source_text, revision)?;
          let workspace_handle = self.workspaces.read().await.workspace_for_document(document_url);
          let mut workspace_state = workspace_handle.write().await;
          document.included = !workspace_state.document_is_excluded(&self.env, document_url)?;
          if document.included {
            workspace_state
              .schemas
              .associations()
              .add_from_document(document_url, &document.dom)?;
          } else {
            workspace_state.schemas.associations().remove_from_document(document_url);
          }
          let installed = match workspace_state.documents.entry(document_url.clone()) {
            Entry::Occupied(mut occupied) => {
              let previous = occupied.insert(document);
              drop(previous);
              occupied.into_mut()
            }
            Entry::Vacant(vacant) => vacant.insert(document),
          };
          let disposition = DocumentDisposition::from(installed.included);
          workspace_state.document_revision = revision;
          let update = DocumentUpdate {
            disposition,
            notifications: workspace_state.association_notifications(),
          };
          drop(workspace_state);
          self.commit_revision(revision);
          Ok(update)
        })
      }

      /// Remove one closed document and its document-owned schema associations.
      pub(crate) fn $close_document<'operation>(
        &'operation self,
        document_url: &'operation Url,
      ) -> $future<'operation, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>> {
        Box::pin(async move {
          let _barrier_guard = self.state_barrier.write().await;
          let workspace_handle = self.workspaces.read().await.workspace_for_document(document_url);
          let mut workspace_state = workspace_handle.write().await;
          let Entry::Occupied(occupied) = workspace_state.documents.entry(document_url.clone()) else {
            return Ok(Vec::new());
          };
          let revision = self.candidate_revision()?;
          let closed_document = occupied.remove_entry();
          drop(closed_document);
          workspace_state.schemas.associations().remove_from_document(document_url);
          workspace_state.document_revision = revision;
          let notifications = workspace_state.association_notifications();
          drop(workspace_state);
          self.commit_revision(revision);
          Ok(notifications)
        })
      }

      /// Clone every open document URL and its current inclusion state.
      pub(crate) fn $open_document_dispositions(&self) -> $future<'_, Vec<(Url, DocumentDisposition)>> {
        Box::pin(async move {
          let _barrier = self.state_barrier.read().await;
          let handles = self.workspaces.read().await.all_handles();
          let mut documents = Vec::new();
          for handle in handles {
            documents.extend(
              handle
                .read()
                .await
                .documents
                .iter()
                .map(|(url, document)| (url.clone(), DocumentDisposition::from(document.included))),
            );
          }
          documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
          documents
        })
      }

      /// List non-document schema associations for the workspace owning one document.
      pub(crate) fn $list_schema_associations<'operation>(
        &'operation self,
        document: &'operation Url,
      ) -> $future<'operation, Vec<SchemaAssociation>> {
        Box::pin(async move {
          self
            .$with_document_workspace(document, |handle| async move {
              handle
                .read()
                .await
                .schemas
                .associations()
                .read()
                .iter()
                .filter(|association_entry| !matches!(association_entry.0, AssociationRule::Url(_)))
                .map(|association_entry| association_entry.1.clone())
                .collect()
            })
            .await
        })
      }

      /// Return the selected schema association for one document.
      pub(crate) fn $associated_schema<'operation>(
        &'operation self,
        document: &'operation Url,
      ) -> $future<'operation, Option<SchemaAssociation>> {
        Box::pin(async move {
          self
            .$with_document_workspace(document, |handle| async move {
              handle.read().await.schemas.associations().association_for(document)
            })
            .await
        })
      }

      /// Apply one validated manual association under the global mutation barrier.
      ///
      /// # Errors
      ///
      /// Returns [`WorldError`] when the supplied glob or regular expression is invalid or the
      /// world revision is exhausted.
      pub(crate) fn $associate_schema(
        &self,
        rule: ManualAssociationRule,
        association: SchemaAssociation,
      ) -> $future<'_, Result<ManualAssociationUpdate, WorldError>> {
        Box::pin(async move {
          let compiled = match rule {
            ManualAssociationRule::Glob(pattern) => CompiledManualAssociation::Global(AssociationRule::glob(&pattern)?),
            ManualAssociationRule::Regex(pattern) => {
              CompiledManualAssociation::Global(
                AssociationRule::regex(&pattern).map_err(|source| WorldError::ManualAssociationPattern {
                  pattern: pattern.clone(),
                  source,
                })?,
              )
            }
            ManualAssociationRule::Url(document) => CompiledManualAssociation::Document(document),
          };
          let _barrier_guard = self.state_barrier.write().await;
          let revision = self.candidate_revision()?;
          let mut notifications = Vec::new();
          let diagnostic_document = match compiled {
            CompiledManualAssociation::Global(compiled_rule) => {
              let handles = self.workspaces.read().await.all_handles();
              for handle in handles {
                let mut workspace_state = handle.write().await;
                workspace_state
                  .schemas
                  .associations()
                  .add(compiled_rule.clone(), association.clone());
                workspace_state.schema_revision = revision;
                notifications.extend(workspace_state.association_notifications());
              }
              None
            }
            CompiledManualAssociation::Document(document) => {
              let handle = self.workspaces.read().await.workspace_for_document(&document);
              let mut workspace_state = handle.write().await;
              workspace_state
                .schemas
                .associations()
                .retain(|association_entry| match association_entry.0 {
                  AssociationRule::Url(ref url) => {
                    url != &document
                      || association_entry.1.meta.get("source").and_then(Value::as_str)
                        != Some(source::MANUAL)
                  }
                  AssociationRule::Glob(_) | AssociationRule::Regex(_) => true,
                });
              workspace_state
                .schemas
                .associations()
                .add(AssociationRule::Url(document.clone()), association);
              workspace_state.schema_revision = revision;
              notifications.extend(workspace_state.association_notifications());
              drop(workspace_state);
              Some(document)
            }
          };
          let update = ManualAssociationUpdate {
            notifications,
            diagnostic_document,
          };
          self.commit_revision(revision);
          Ok(update)
        })
      }
    }
  };
}

use crate::LocalFuture;

define_world_state_future_family!(
  future = LocalFuture,
  environment_bounds = [],
  transport_bounds = [],
  operation_bounds = [],
  with_document_workspace = with_document_workspace,
  document_snapshot = document_snapshot,
  snapshot_is_current = snapshot_is_current,
  rooted_workspace_urls = rooted_workspace_urls,
  plan_topology_change = plan_topology_change,
  replace_document = replace_document,
  close_document = close_document,
  open_document_dispositions = open_document_dispositions,
  list_schema_associations = list_schema_associations,
  associated_schema = associated_schema,
  associate_schema = associate_schema,
);

#[cfg(not(target_arch = "wasm32"))]
use crate::ConcurrentFuture;

#[cfg(not(target_arch = "wasm32"))]
define_world_state_future_family!(
  future = ConcurrentFuture,
  environment_bounds = [Send, Sync],
  transport_bounds = [Send, Sync],
  operation_bounds = [Send],
  with_document_workspace = with_document_workspace_concurrent,
  document_snapshot = document_snapshot_concurrent,
  snapshot_is_current = snapshot_is_current_concurrent,
  rooted_workspace_urls = rooted_workspace_urls_concurrent,
  plan_topology_change = plan_topology_change_concurrent,
  replace_document = replace_document_concurrent,
  close_document = close_document_concurrent,
  open_document_dispositions = open_document_dispositions_concurrent,
  list_schema_associations = list_schema_associations_concurrent,
  associated_schema = associated_schema_concurrent,
  associate_schema = associate_schema_concurrent,
);

/// Insert one document while preserving the one-workspace-per-document invariant.
fn insert_unique_document(
  documents: &mut HashMap<Url, DocumentState>,
  document_url: Url,
  document: DocumentState,
) -> Result<(), WorldError> {
  match documents.entry(document_url) {
    Entry::Vacant(vacant) => {
      let _: &mut DocumentState = vacant.insert(document);
      Ok(())
    }
    Entry::Occupied(occupied) => Err(WorldError::DuplicateDocumentOwnership {
      document: occupied.key().clone(),
    }),
  }
}

/// Merge documents from one workspace seed without allowing silent replacement.
fn merge_unique_documents(documents: &mut HashMap<Url, DocumentState>, incoming: HashMap<Url, DocumentState>) -> Result<(), WorldError> {
  let mut incoming_documents: Vec<_> = incoming.into_iter().collect();
  incoming_documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
  for (document_url, document) in incoming_documents {
    insert_unique_document(documents, document_url, document)?;
  }
  Ok(())
}

/// A manual schema-association rule before validation.
pub(crate) enum ManualAssociationRule {
  /// A cross-workspace glob expression.
  Glob(String),
  /// A cross-workspace regular expression.
  Regex(String),
  /// One exact document URL.
  Url(Url),
}

/// A validated manual association target.
enum CompiledManualAssociation {
  /// One rule applied to every workspace.
  Global(AssociationRule),
  /// One exact document association.
  Document(Url),
}

/// Outcomes of a committed manual association update.
#[derive(Debug)]
pub(crate) struct ManualAssociationUpdate {
  /// Updated document-to-schema notifications.
  pub(crate) notifications:       Vec<DidChangeSchemaAssociationParams>,
  /// Exact document whose diagnostics should be refreshed.
  pub(crate) diagnostic_document: Option<Url>,
}

/// Complete inputs for constructing one isolated workspace replacement.
struct WorkspacePreparation<T: SchemaTransport> {
  /// Semantic workspace seed.
  seed:           WorkspaceSeed,
  /// Schema transport owned by the replacement.
  transport:      T,
  /// Host or embedding default configuration.
  default_config: Arc<Config>,
  /// Persistent cache path.
  cache_path:     Option<PathBuf>,
  /// Candidate revision published only after the complete transaction succeeds.
  revision:       Revision,
}

/// Generate isolated workspace preparation for one execution family.
macro_rules! define_workspace_preparation_future_family {
  (
    execution =
    ($future:ident, $environment:path, $transport:ident); preparation =
    {
      loaded:
      $prepare_loaded:ident,from_host:
      $prepare_from_host:ident,initialize:
      $initialize:ident,apply_configuration:
      $apply_configuration:ident $(,)?
    };
  ) => {
    impl<E: $environment> WorkspacePreparation<$transport<E>> {
      /// Prepare using the seed's already loaded configuration.
      fn $prepare_loaded(self, environment: &E) -> $future<'_, Result<WorkspaceState<$transport<E>>, WorldError>> {
        Box::pin(async move {
          let configuration = self.seed.taplo_config.clone();
          let mut state = self.seed.into_state(self.transport, self.cache_path)?;
          drop(state.$apply_configuration(environment, configuration, self.revision).await?);
          Ok(state)
        })
      }

      /// Discover, read, and prepare configuration through this execution family's host
      /// capabilities.
      fn $prepare_from_host(self, environment: &E) -> $future<'_, Result<WorkspaceState<$transport<E>>, WorldError>> {
        Box::pin(async move {
          let mut state = self.seed.into_state(self.transport, self.cache_path)?;
          drop(state.$initialize(environment, &self.default_config, self.revision).await?);
          Ok(state)
        })
      }
    }
  };
}

define_workspace_preparation_future_family!(
  execution = (LocalFuture, LocalEnvironment, LocalSchemaTransport);
  preparation = {
    loaded: prepare_loaded_local,
    from_host: prepare_local,
    initialize: initialize_local,
    apply_configuration: apply_configuration_local,
  };
);

#[cfg(not(target_arch = "wasm32"))]
define_workspace_preparation_future_family!(
  execution = (
    ConcurrentFuture,
    ConcurrentEnvironment,
    ConcurrentSchemaTransport
  );
  preparation = {
    loaded: prepare_loaded_concurrent,
    from_host: prepare_concurrent,
    initialize: initialize_concurrent,
    apply_configuration: apply_configuration_concurrent,
  };
);

/// Owns one world mutation from isolated preparation through atomic publication.
struct WorldTransaction<'world, E: Environment, T: SchemaTransport, P> {
  /// World whose mutation barrier and committed state this transaction coordinates.
  world:    &'world WorldState<E, T>,
  /// Candidate preparation strategy retained for the complete transaction.
  preparer: P,
}

impl<'world, E: Environment, T: SchemaTransport, P> WorldTransaction<'world, E, T, P> {
  /// Begin a transaction against `world` without acquiring its mutation barrier yet.
  const fn new(world: &'world WorldState<E, T>, preparer: P) -> Self {
    Self {
      world,
      preparer,
    }
  }

  /// Bundle one seed with immutable transaction inputs.
  fn workspace_preparation(
    &self,
    seed: WorkspaceSeed,
    default_config: Arc<Config>,
    cache_path: Option<PathBuf>,
    revision: Revision,
  ) -> WorkspacePreparation<T> {
    WorkspacePreparation {
      seed,
      transport: self.world.transport.clone(),
      default_config,
      cache_path,
      revision,
    }
  }
}

/// Prepare one isolated workspace candidate for later atomic publication.
trait WorkspacePreparer<T: SchemaTransport> {
  /// Future resolving the isolated candidate.
  type Future: Future<Output = Result<WorkspaceState<T>, WorldError>> + 'static;

  /// Prepare one owned transaction input.
  fn prepare(&mut self, preparation: WorkspacePreparation<T>) -> Self::Future;
}

impl<T, F, Fut> WorkspacePreparer<T> for F
where
  T: SchemaTransport,
  F: FnMut(WorkspacePreparation<T>) -> Fut,
  Fut: Future<Output = Result<WorkspaceState<T>, WorldError>> + 'static,
{
  type Future = Fut;

  fn prepare(&mut self, preparation: WorkspacePreparation<T>) -> Self::Future {
    self(preparation)
  }
}

/// Generate atomic world transactions and their world-facing adapters for one execution family.
macro_rules! define_world_transaction_future_family {
  (
    $transaction_change_roots:ident,
    $transaction_initialize_roots:ident,
    $transaction_apply_configuration:ident,
    $world_change_roots:ident,
    $world_initialize_roots:ident,
    $world_apply_configuration:ident,
    $plan_topology_change:ident,
    $configuration_candidates:ident,
    $prepare_loaded:ident,
    $prepare_from_host:ident,
    $host_preparer:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Prepare workspace candidates through one execution family's host capabilities.
    struct $host_preparer<E: $environment>(E);

    impl<E: $environment> WorkspacePreparer<$transport<E>> for $host_preparer<E> {
      type Future = $future<'static, Result<WorkspaceState<$transport<E>>, WorldError>>;

      fn prepare(&mut self, preparation: WorkspacePreparation<$transport<E>>) -> Self::Future {
        let environment = self.0.clone();
        Box::pin(async move { preparation.$prepare_from_host(&environment).await })
      }
    }

    impl<'world, E, P> WorldTransaction<'world, E, $transport<E>, P>
    where
      E: $environment,
      P: WorkspacePreparer<$transport<E>> $(+ $value_bound)* + 'world,
      P::Future: $($value_bound +)* 'world,
    {
      /// Prepare and publish a topology change through this execution family's loader.
      fn $transaction_change_roots(
        mut self,
        removed: &'world [Url],
        added: &'world [Url],
      ) -> $future<'world, Result<TopologyUpdate, WorldError>>
      {
        Box::pin(async move {
          let _barrier = self.world.state_barrier.write().await;
          let Some(plan) = self.world.$plan_topology_change(removed, added).await? else {
            return Ok(TopologyUpdate {
              notifications: Vec::new(),
              documents:     Vec::new(),
            });
          };
          let revision = self.world.candidate_revision()?;
          let cache_path = self.world.init_config().cache_path.clone();
          let default_config = self.world.default_config();
          let detached = self
            .workspace_preparation(
              plan.detached,
              Arc::clone(&default_config),
              cache_path.clone(),
              revision,
            )
            .$prepare_loaded(&self.world.env)
            .await?;
          let mut rooted = Vec::with_capacity(plan.rooted.len());
          for planned in plan.rooted {
            let preparation = self.workspace_preparation(
              planned.seed,
              Arc::clone(&default_config),
              cache_path.clone(),
              revision,
            );
            let state = if planned.load_from_host {
              self.preparer.prepare(preparation).await?
            } else {
              preparation.$prepare_loaded(&self.world.env).await?
            };
            rooted.push((planned.root, state));
          }

          let prepared = PreparedTopology::new(detached, rooted, plan.documents);
          *self.world.workspaces.write().await = prepared.workspaces;
          self.world.commit_revision(revision);
          Ok(TopologyUpdate {
            notifications: prepared.notifications,
            documents:     prepared.documents,
          })
        })
      }

      /// Prepare and publish initial roots through this execution family's loader.
      fn $transaction_initialize_roots(
        mut self,
        init_config: Arc<InitConfig>,
        roots: Vec<Url>,
      ) -> $future<'world, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>>
      {
        Box::pin(async move {
          let _barrier = self.world.state_barrier.write().await;
          let effective_init_config = self.world.effective_init_config(init_config);
          if !self.world.workspaces.read().await.rooted.is_empty() {
            return Err(WorldError::AlreadyInitialized);
          }
          ensure_unique_roots(&roots)?;
          let revision = self.world.candidate_revision()?;
          let default_config = self.world.default_config();
          let mut prepared = Vec::with_capacity(roots.len());
          for root in roots {
            let preparation = self.workspace_preparation(
              WorkspaceSeed::empty(WorkspaceRoot::Rooted(root.clone()))?,
              Arc::clone(&default_config),
              effective_init_config.cache_path.clone(),
              revision,
            );
            prepared.push((root, self.preparer.prepare(preparation).await?));
          }

          let mut notifications = Vec::new();
          for prepared_entry in &prepared {
            notifications.extend(prepared_entry.1.association_notifications());
          }
          let prepared_roots = prepared
            .into_iter()
            .map(|(root, state)| (root, Arc::new(AsyncRwLock::new(state))))
            .collect();
          let mut topology = self.world.workspaces.write().await;
          if !topology.rooted.is_empty() {
            return Err(WorldError::AlreadyInitialized);
          }
          topology
            .detached
            .write()
            .await
            .schemas
            .cache()
            .set_cache_path(effective_init_config.cache_path.clone());
          topology.rooted = prepared_roots;
          drop(topology);
          *self.world.init_config.write() = effective_init_config;
          self.world.commit_revision(revision);
          Ok(notifications)
        })
      }

      /// Prepare and publish configuration replacements through this execution family's loader.
      fn $transaction_apply_configuration(
        mut self,
        global: Option<&'world Value>,
        scoped: &'world [(Url, Value)],
      ) -> $future<'world, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>>
      {
        Box::pin(async move {
          let _barrier = self.world.state_barrier.write().await;
          let candidates = $configuration_candidates(&self.world.workspaces, global, scoped).await?;
          if candidates.is_empty() {
            return Ok(Vec::new());
          }
          let revision = self.world.candidate_revision()?;
          let default_config = self.world.default_config();
          let cache_path = self.world.init_config().cache_path.clone();
          let mut replacements = Vec::with_capacity(candidates.len());
          for candidate in candidates {
            let mut seed = candidate.handle.read().await.seed();
            seed.config = candidate.configuration;
            let preparation = self.workspace_preparation(
              seed,
              Arc::clone(&default_config),
              cache_path.clone(),
              revision,
            );
            replacements.push((
              candidate.handle,
              self.preparer.prepare(preparation).await?,
            ));
          }
          let mut notifications = Vec::new();
          for (handle, replacement) in replacements {
            notifications.extend(replacement.association_notifications());
            *handle.write().await = replacement;
          }
          self.world.commit_revision(revision);
          Ok(notifications)
        })
      }
    }

    impl<E: $environment> WorldState<E, $transport<E>> {
      /// Prepare and atomically replace workspace topology through this execution family's host
      /// capabilities.
      ///
      /// # Errors
      ///
      /// Returns [`WorldError`] when any new or retained workspace cannot be prepared.
      pub(crate) fn $world_change_roots<'operation>(
        &'operation self,
        removed: &'operation [Url],
        added: &'operation [Url],
      ) -> $future<'operation, Result<TopologyUpdate, WorldError>> {
        WorldTransaction::new(self, $host_preparer(self.env.clone())).$transaction_change_roots(removed, added)
      }

      /// Prepare and atomically install the complete initial rooted topology.
      ///
      /// # Errors
      ///
      /// Returns [`WorldError`] when roots are duplicated, initialization already occurred, or
      /// any workspace configuration/schema preparation fails.
      pub(crate) fn $world_initialize_roots(
        &self,
        init_config: Arc<InitConfig>,
        roots: Vec<Url>,
      ) -> $future<'_, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>> {
        WorldTransaction::new(self, $host_preparer(self.env.clone())).$transaction_initialize_roots(init_config, roots)
      }

      /// Apply global and root-scoped client configuration using this execution family's host
      /// capabilities.
      ///
      /// # Errors
      ///
      /// Returns [`WorldError`] when a value is malformed, a captured root is stale, or any
      /// affected workspace cannot be reinitialized.
      pub(crate) fn $world_apply_configuration<'operation>(
        &'operation self,
        global: Option<&'operation Value>,
        scoped: &'operation [(Url, Value)],
      ) -> $future<'operation, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>> {
        WorldTransaction::new(self, $host_preparer(self.env.clone())).$transaction_apply_configuration(global, scoped)
      }
    }
  };
}

/// One validated configuration replacement awaiting workspace initialization.
struct WorkspaceConfigCandidate<T: SchemaTransport> {
  /// Target workspace.
  handle:        WorkspaceHandle<T>,
  /// Fully decoded replacement configuration.
  configuration: LspConfig,
}

/// Generate configuration-candidate capture for one execution family.
macro_rules! define_configuration_candidates_future_family {
  (
    $configuration_candidates:ident; $future:ident, $environment:path, $transport:ident, $schema_execution:path; [$($value_bound:path),*]
  ) => {
    /// Validate global and root-scoped configuration values before any live update.
    #[allow(
      clippy::single_call_fn,
      reason = "the named validation step is what keeps the complete shape, ownership, and merge checks ahead of the transaction that \
                publishes them, once per execution family"
    )]
    fn $configuration_candidates<'operation, E: $environment>(
      workspaces: &'operation AsyncRwLock<Workspaces<$transport<E>>>,
      global: Option<&'operation Value>,
      scoped: &'operation [(Url, Value)],
    ) -> $future<'operation, Result<Vec<WorkspaceConfigCandidate<$transport<E>>>, WorldError>> {
      Box::pin(async move {
        if global.is_some_and(|global_configuration| !global_configuration.is_object()) {
          return Err(WorldError::ConfigurationResponse {
            scope:  "global workspace configuration".into(),
            reason: "expected an object",
          });
        }
        for scoped_entry in scoped {
          if !scoped_entry.1.is_object() {
            return Err(WorldError::ConfigurationResponse {
              scope:  scoped_entry.0.to_string(),
              reason: "expected an object",
            });
          }
        }

        let topology = workspaces.read().await;
        let global_handles = global.map_or_else(Vec::new, |_| topology.all_handles());
        let mut scoped_handles = Vec::with_capacity(scoped.len());
        for scoped_entry in scoped {
          let handle = topology.rooted(&scoped_entry.0).ok_or_else(|| WorldError::MissingWorkspace {
            root: scoped_entry.0.clone(),
          })?;
          scoped_handles.push((handle, &scoped_entry.1));
        }
        drop(topology);

        let mut candidates = Vec::new();
        if let Some(global_configuration) = global {
          for handle in global_handles {
            let mut configuration = handle.read().await.config.clone();
            configuration.update_from_json(global_configuration)?;
            candidates.push(WorkspaceConfigCandidate {
              handle,
              configuration,
            });
          }
        }

        for (handle, scoped_configuration) in scoped_handles {
          if let Some(candidate) = candidates.iter_mut().find(|candidate| Arc::ptr_eq(&candidate.handle, &handle)) {
            candidate.configuration.update_from_json(scoped_configuration)?;
          } else {
            let mut configuration = handle.read().await.config.clone();
            configuration.update_from_json(scoped_configuration)?;
            candidates.push(WorkspaceConfigCandidate {
              handle,
              configuration,
            });
          }
        }
        Ok(candidates)
      })
    }
  };
}

define_lsp_execution_families!(
  world
  define_configuration_candidates_future_family;
  (configuration_candidates_local, configuration_candidates_concurrent),
);

define_lsp_execution_families!(
  world
  define_world_transaction_future_family;
  (change_roots_with, change_roots_with_concurrent),
  (initialize_roots_with, initialize_roots_with_concurrent),
  (
    apply_configuration_values_with,
    apply_configuration_values_with_concurrent
  ),
  (change_roots_local, change_roots_concurrent),
  (initialize_roots_local, initialize_roots_concurrent),
  (
    apply_configuration_values_local,
    apply_configuration_values_concurrent
  ),
  (plan_topology_change, plan_topology_change_concurrent),
  (
    configuration_candidates_local,
    configuration_candidates_concurrent
  ),
  (prepare_loaded_local, prepare_loaded_concurrent),
  (prepare_local, prepare_concurrent),
  (
    LocalHostWorkspacePreparer,
    ConcurrentHostWorkspacePreparer
  ),
);

/// Mutable state owned by one detached or rooted workspace.
#[cfg_attr(test, derive(Debug))]
pub(crate) struct WorkspaceState<T: SchemaTransport> {
  /// Workspace domain root.
  pub(crate) root:         WorkspaceRoot,
  /// Open documents owned by this workspace.
  pub(crate) documents:    HashMap<Url, DocumentState>,
  /// Prepared Taplo configuration.
  pub(crate) taplo_config: Config,
  /// Schema services and associations.
  pub(crate) schemas:      Schemas<T>,
  /// LSP configuration.
  pub(crate) config:       LspConfig,
  /// Last document-set mutation.
  document_revision:       Revision,
  /// Last configuration mutation.
  config_revision:         Revision,
  /// Last schema/association mutation.
  schema_revision:         Revision,
}

/// Cloneable semantic workspace inputs used to prepare an isolated replacement.
struct WorkspaceSeed {
  /// Workspace domain root.
  root:                WorkspaceRoot,
  /// Every currently open document, including documents excluded by active rules.
  documents:           HashMap<Url, DocumentState>,
  /// Replacement LSP configuration.
  config:              LspConfig,
  /// Last successfully prepared Taplo configuration.
  taplo_config:        Config,
  /// Last document-set mutation retained across configuration replacement.
  document_revision:   Revision,
  /// Associations owned explicitly by the client.
  manual_associations: Vec<(AssociationRule, SchemaAssociation)>,
}

impl WorkspaceSeed {
  /// Construct an empty rooted or detached semantic seed.
  fn empty(root: WorkspaceRoot) -> Result<Self, WorldError> {
    Ok(Self {
      root,
      documents: HashMap::default(),
      config: LspConfig::new()?,
      taplo_config: Config::default(),
      document_revision: Revision::INITIAL,
      manual_associations: Vec::new(),
    })
  }

  /// Construct a fresh isolated state around this semantic seed.
  fn into_state<T: SchemaTransport>(self, transport: T, cache_path: Option<PathBuf>) -> Result<WorkspaceState<T>, WorldError> {
    let mut state = WorkspaceState::new(self.root, transport)?;
    state.documents = self.documents;
    state.config = self.config;
    state.document_revision = self.document_revision;
    state.schemas.cache().set_cache_path(cache_path);
    for (rule, association) in self.manual_associations {
      state.schemas.associations().add(rule, association);
    }
    Ok(state)
  }
}

impl<T: SchemaTransport> WorkspaceState<T> {
  /// Construct one workspace against an explicit transport capability.
  fn new(root: WorkspaceRoot, transport: T) -> Result<Self, WorldError> {
    Ok(Self {
      root,
      documents: HashMap::default(),
      taplo_config: Config::default(),
      schemas: Schemas::with_transport(transport)?,
      config: LspConfig::new()?,
      document_revision: Revision::INITIAL,
      config_revision: Revision::INITIAL,
      schema_revision: Revision::INITIAL,
    })
  }

  /// Capture the semantic inputs needed to prepare an isolated replacement.
  fn seed(&self) -> WorkspaceSeed {
    let manual_associations = self
      .schemas
      .associations()
      .read()
      .iter()
      .filter(|association_entry| association_entry.1.meta.get("source").and_then(Value::as_str) == Some(source::MANUAL))
      .cloned()
      .collect();
    WorkspaceSeed {
      root: self.root.clone(),
      documents: self.documents.clone(),
      config: self.config.clone(),
      taplo_config: self.taplo_config.clone(),
      document_revision: self.document_revision,
      manual_associations,
    }
  }

  /// Clone the document and schema/config inputs needed by async handlers.
  fn document_snapshot(&self, url: &Url) -> Option<DocumentSnapshot<T>> {
    let document = self.documents.get(url)?.clone();
    if !document.included {
      return None;
    }
    Some(DocumentSnapshot {
      document,
      schemas: self.schemas.clone(),
      config: self.config.clone(),
      taplo_config: self.taplo_config.clone(),
      config_revision: self.config_revision,
      schema_revision: self.schema_revision,
    })
  }

  /// Return whether a file document is excluded by a prepared workspace rule.
  fn document_is_excluded(&self, environment: &impl Environment, document: &Url) -> Result<bool, WorldError> {
    document_is_excluded(environment, &self.taplo_config, document)
  }

  /// Build current association notifications without performing client output.
  fn association_notifications(&self) -> Vec<DidChangeSchemaAssociationParams> {
    let mut documents: Vec<_> = self.documents.iter().collect();
    documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    documents
      .into_iter()
      .map(|document_entry| {
        let association = if self.config.schema.enabled && document_entry.1.included {
          self.schemas.associations().association_for(document_entry.0)
        } else {
          None
        };
        DidChangeSchemaAssociationParams {
          document_uri: document_entry.0.clone(),
          schema_uri:   association.as_ref().map(|association_details| association_details.url.clone()),
          meta:         association.map(|association_details| association_details.meta),
        }
      })
      .collect()
  }
}

/// Generate configuration initialization and commit operations for one execution family.
macro_rules! define_workspace_state_future_family {
  (
    $initialize:ident,
    $apply_configuration:ident,
    $load_config:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    impl<E: $environment> WorkspaceState<$transport<E>> {
      /// Initialize from configuration using this execution family's host.
      fn $initialize<'operation>(
        &'operation mut self,
        environment: &'operation E,
        default_config: &'operation Config,
        revision: Revision,
      ) -> $future<'operation, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>> {
        Box::pin(async move {
          let configuration =
            $load_config(environment, &self.root, &self.config, default_config).await?;
          self
            .$apply_configuration(environment, configuration, revision)
            .await
        })
      }

      /// Validate and commit prepared configuration and every owned association source.
      fn $apply_configuration<'operation>(
        &'operation mut self,
        environment: &'operation E,
        configuration: Config,
        revision: Revision,
      ) -> $future<'operation, Result<Vec<DidChangeSchemaAssociationParams>, WorldError>> {
        Box::pin(async move {
          let mut lsp_associations = Vec::new();
          let mut configured_associations: Vec<_> =
            self.config.schema.associations.iter().collect();
          configured_associations
            .sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
          for configured_association in configured_associations {
            let pattern = configured_association.0;
            let schema_url = configured_association.1;
            let regex =
              Regex::new(pattern).map_err(|source| WorldError::AssociationPattern {
                pattern: pattern.clone(),
                source,
              })?;
            let url = if schema_url.starts_with("./") {
              self
                .root
                .url()
                .ok_or_else(|| WorldError::DetachedRelativePath {
                  path: PathBuf::from(schema_url),
                })?
                .join(schema_url)
                .map_err(|source| WorldError::AssociationUrl {
                  source_value: schema_url.clone(),
                  source,
                })?
            } else {
              schema_url.parse().map_err(|source| WorldError::AssociationUrl {
                source_value: schema_url.clone(),
                source,
              })?
            };
            lsp_associations.push((AssociationRule::Regex(regex), SchemaAssociation {
              url,
              meta: json!({ "source": source::LSP_CONFIG }),
              priority: priority::LSP_CONFIG,
            }));
          }

          self.schemas.cache().set_expiration_times(
            Duration::from_secs(self.config.schema.cache.memory_expiration),
            Duration::from_secs(self.config.schema.cache.disk_expiration),
          )?;
          if self.config.schema.enabled {
            <$schema_execution>::replace_catalogs(
              self.schemas.associations(),
              &self.config.schema.catalogs,
            )
            .await?;
          } else {
            <$schema_execution>::replace_catalogs(self.schemas.associations(), &[]).await?;
            lsp_associations.clear();
          }

          self.schemas.associations().add_from_config(&configuration);
          self
            .schemas
            .associations()
            .replace_source(source::LSP_CONFIG, lsp_associations);
          let mut document_inclusion = Vec::with_capacity(self.documents.len());
          let mut documents: Vec<_> = self.documents.iter().collect();
          documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
          for document_entry in documents {
            document_inclusion.push((
              document_entry.0.clone(),
              !document_is_excluded(environment, &configuration, document_entry.0)?,
              document_entry.1.dom.clone(),
            ));
          }
          for (document_url, included, dom) in document_inclusion {
            if let Some(document) = self.documents.get_mut(&document_url) {
              document.included = included;
            }
            if included {
              self
                .schemas
                .associations()
                .add_from_document(&document_url, &dom)?;
            } else {
              self
                .schemas
                .associations()
                .remove_from_document(&document_url);
            }
          }
          self.taplo_config = configuration;
          self.config_revision = revision;
          self.schema_revision = revision;
          Ok(self.association_notifications())
        })
      }
    }
  };
}

define_lsp_execution_families!(
  world
  define_workspace_state_future_family;
  (initialize_local, initialize_concurrent),
  (apply_configuration_local, apply_configuration_concurrent),
  (load_config_local, load_config_concurrent),
);

/// Return whether one document is excluded by a prepared configuration.
fn document_is_excluded(environment: &impl Environment, configuration: &Config, document: &Url) -> Result<bool, WorldError> {
  let Some(path) = environment.to_file_path_normalized(document)? else {
    return Ok(false);
  };
  Ok(configuration.file_rule.as_ref().is_some_and(|rule| !rule.is_match(path)))
}

/// Generate one configuration loader around execution-family-specific host calls.
macro_rules! define_config_loader {
  (
    name =
    $name:ident,future =
    $future:ident,environment =
    $environment:path,read = |
    $read_environment:ident,
    $read_path:ident |
    $read:expr,discover = |
    $discover_environment:ident,
    $discover_base:ident |
    $discover:expr,
  ) => {
    /// Load and prepare one workspace configuration through this host execution family.
    #[allow(
      clippy::single_call_fn,
      reason = "each host execution family expands one configuration loader, called once by that family's workspace initialization; the \
                shared `ConfigSourcePlan` decision is what both expansions reuse"
    )]
    fn $name<'operation, E: $environment>(
      environment: &'operation E,
      root: &'operation WorkspaceRoot,
      lsp_config: &'operation LspConfig,
      default_config: &'operation Config,
    ) -> $future<'operation, Result<Config, WorldError>> {
      Box::pin(async move {
        let plan = ConfigSourcePlan::new(environment, root, lsp_config)?;
        let base_path = plan.base_path().to_path_buf();
        let loaded = match plan {
          ConfigSourcePlan::Disabled {
            ..
          }
          | ConfigSourcePlan::DetachedDefault {
            ..
          } => None,
          ConfigSourcePlan::ExplicitAbsolute {
            path, ..
          }
          | ConfigSourcePlan::RootedRelative {
            path, ..
          } => {
            let selected_path = path.clone();
            let $read_environment = environment;
            let $read_path = path;
            Some((selected_path, $read.await?))
          }
          ConfigSourcePlan::RootedDiscovery {
            base_path: discovery_base,
          } => {
            let $discover_environment = environment;
            let $discover_base = discovery_base;
            match $discover.await? {
              Some(path) => {
                let selected_path = path.clone();
                let $read_environment = environment;
                let $read_path = path;
                Some((selected_path, $read.await?))
              }
              None => None,
            }
          }
        };
        prepare_loaded_config(environment, lsp_config, default_config, &base_path, loaded)
      })
    }
  };
}

define_config_loader!(
  name = load_config_local,
  future = LocalFuture,
  environment = LocalEnvironment,
  read = |host, path| host.read_file(&path),
  discover = |host, base| host.find_config_file_normalized(&base),
);

#[cfg(not(target_arch = "wasm32"))]
define_config_loader!(
  name = load_config_concurrent,
  future = ConcurrentFuture,
  environment = ConcurrentEnvironment,
  read = |host, path| host.read_file_concurrent(path),
  discover = |host, base| host.find_config_file_concurrent(base),
);

/// Resolve the workspace base path without asynchronous host calls.
#[allow(
  clippy::single_call_fn,
  reason = "the named resolver isolates the rooted-versus-detached base-path rule and its two typed failures, so `ConfigSourcePlan::new` \
            stays a synchronous classification with no host I/O"
)]
fn workspace_base_path(environment: &impl Environment, root: &WorkspaceRoot) -> Result<PathBuf, WorldError> {
  match *root {
    WorkspaceRoot::Rooted(ref root_url) => environment
      .to_file_path_normalized(root_url)?
      .ok_or_else(|| WorldError::InvalidWorkspaceRoot {
        root: root_url.clone()
      }),
    WorkspaceRoot::Detached => environment.cwd_normalized()?.ok_or(WorldError::MissingCurrentDirectory),
  }
}

/// Synchronous configuration-source decision shared by local and concurrent hosts.
enum ConfigSourcePlan {
  /// Configuration loading is disabled.
  Disabled {
    /// Workspace base used to prepare the default configuration.
    base_path: PathBuf,
  },
  /// Client supplied an absolute path.
  ExplicitAbsolute {
    /// Workspace base used to prepare the loaded configuration.
    base_path: PathBuf,
    /// Absolute configuration path.
    path:      PathBuf,
  },
  /// Client supplied a path relative to a rooted workspace.
  RootedRelative {
    /// Workspace base used to prepare the loaded configuration.
    base_path: PathBuf,
    /// Resolved configuration path.
    path:      PathBuf,
  },
  /// A rooted workspace should discover a configuration file.
  RootedDiscovery {
    /// Root from which discovery starts and configuration is prepared.
    base_path: PathBuf,
  },
  /// A detached workspace uses the default configuration without discovery.
  DetachedDefault {
    /// Current directory used to prepare the default configuration.
    base_path: PathBuf,
  },
}

impl ConfigSourcePlan {
  /// Classify configuration loading before any asynchronous host operation.
  fn new(environment: &impl Environment, root: &WorkspaceRoot, lsp_config: &LspConfig) -> Result<Self, WorldError> {
    let base_path = workspace_base_path(environment, root)?;
    if !lsp_config.taplo.config_file.enabled {
      return Ok(Self::Disabled {
        base_path,
      });
    }
    let Some(config_path) = lsp_config.taplo.config_file.path.as_ref() else {
      return if matches!(root, WorkspaceRoot::Rooted(_)) {
        Ok(Self::RootedDiscovery {
          base_path,
        })
      } else {
        Ok(Self::DetachedDefault {
          base_path,
        })
      };
    };
    if environment.is_absolute(config_path)? {
      return Ok(Self::ExplicitAbsolute {
        base_path,
        path: config_path.clone(),
      });
    }
    if matches!(root, WorkspaceRoot::Rooted(_)) {
      Ok(Self::RootedRelative {
        path: base_path.join(config_path),
        base_path,
      })
    } else {
      Err(WorldError::DetachedRelativePath {
        path: config_path.clone()
      })
    }
  }

  /// Return the base path used for final configuration preparation.
  fn base_path(&self) -> &Path {
    match *self {
      Self::Disabled {
        ref base_path,
      }
      | Self::ExplicitAbsolute {
        ref base_path, ..
      }
      | Self::RootedRelative {
        ref base_path, ..
      }
      | Self::RootedDiscovery {
        ref base_path,
      }
      | Self::DetachedDefault {
        ref base_path,
      } => base_path,
    }
  }
}

/// Decode, merge, and prepare a loaded workspace configuration.
fn prepare_loaded_config(
  environment: &impl Environment,
  lsp_config: &LspConfig,
  default_config: &Config,
  base_path: &Path,
  loaded: Option<(PathBuf, Vec<u8>)>,
) -> Result<Config, WorldError> {
  let mut configuration = match loaded {
    Some((path, bytes)) => {
      let source_text = str::from_utf8(&bytes).map_err(|source| WorldError::ConfigUtf8 {
        path: path.clone(),
        source,
      })?;
      toml::from_str(source_text).map_err(|source| WorldError::ConfigToml {
        path,
        source,
      })?
    }
    None => default_config.clone(),
  };
  configuration.rule.extend(lsp_config.rules.clone());
  configuration.prepare(environment, base_path)?;
  Ok(configuration)
}

/// Cheap immutable inputs for handlers that may await schema resolution or client output.
#[derive(Clone, Debug)]
pub(crate) struct DocumentSnapshot<T: SchemaTransport> {
  /// Parsed immutable document state.
  pub(crate) document:     DocumentState,
  /// Cloneable schema services.
  pub(crate) schemas:      Schemas<T>,
  /// LSP behavior configuration.
  pub(crate) config:       LspConfig,
  /// Prepared Taplo configuration.
  pub(crate) taplo_config: Config,
  /// Captured configuration revision.
  config_revision:         Revision,
  /// Captured schema/association revision.
  schema_revision:         Revision,
}

/// Result of installing a changed document.
#[derive(Debug)]
pub(crate) struct DocumentUpdate {
  /// Whether the document remains owned and parsed.
  pub(crate) disposition:   DocumentDisposition,
  /// Association notifications produced by the committed mutation.
  pub(crate) notifications: Vec<DidChangeSchemaAssociationParams>,
}

/// Whether a document is included by prepared workspace rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DocumentDisposition {
  /// The document remains open and participates in language features.
  Included,
  /// The document remains open but is withheld from language features.
  Excluded,
}

impl From<bool> for DocumentDisposition {
  fn from(included: bool) -> Self {
    if included { Self::Included } else { Self::Excluded }
  }
}

/// Parsed open-document state retained across workspace migration.
#[derive(Clone, Debug)]
pub struct DocumentState {
  /// Lossless parsed syntax and diagnostics.
  pub(crate) parse:  Parse,
  /// Frozen semantic DOM.
  pub(crate) dom:    Node,
  /// Checked source-to-LSP coordinate mapper.
  pub(crate) mapper: Mapper,
  /// Revision at which this source was parsed.
  revision:          Revision,
  /// Whether prepared workspace rules currently include this open document.
  included:          bool,
}

impl DocumentState {
  /// Parse and retain one complete document snapshot at the initial revision.
  ///
  /// # Errors
  ///
  /// Returns [`WorldError`] when syntax-tree or coordinate construction fails.
  #[cfg(test)]
  pub(crate) fn parse(source: &str) -> Result<Self, WorldError> {
    Self::parse_at(source, Revision::INITIAL)
  }

  /// Parse and retain one document at a committed world revision.
  fn parse_at(source: &str, revision: Revision) -> Result<Self, WorldError> {
    let parse = parse(source)?;
    let dom = parse.clone().into_dom();
    Ok(Self {
      parse,
      dom,
      mapper: Mapper::new_utf16(source)?,
      revision,
      included: true,
    })
  }
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::path::Path;
  use std::path::PathBuf;
  use std::slice::from_ref;
  use std::sync::Arc;
  use std::sync::atomic::Ordering;

  use futures::executor::block_on;
  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo_common::HashMap;
  use taplo_common::config::Config;
  use taplo_common::environment::EnvironmentError;
  use taplo_common::schema::associations::SchemaAssociation;
  use taplo_common::schema::associations::priority;
  use taplo_common::schema::associations::source;
  use taplo_common::schema::cache::CacheError;
  #[cfg(not(target_arch = "wasm32"))]
  use taplo_common::schema::transport::ConcurrentSchemaTransport;
  use taplo_common::schema::transport::LocalSchemaTransport;
  use taplo_common::schema::transport::SchemaTransport;
  use taplo_common::schema::transport::TransportError;
  use taplo_common::schema::transport::local_http_client;
  use thiserror::Error;
  use url::ParseError;
  use url::Url;

  use super::DocumentDisposition;
  use super::DocumentSnapshot;
  use super::DocumentState;
  use super::DocumentUpdate;
  use super::ManualAssociationRule;
  use super::Revision;
  use super::TestEnvironment;
  use super::WorkspacePreparation;
  use super::WorkspaceRoot;
  use super::WorkspaceState;
  use super::WorldError;
  use super::WorldState;
  use super::WorldTransaction;
  use super::deepest_root;
  use super::ensure_unique_roots;
  use super::insert_unique_document;
  #[cfg(not(target_arch = "wasm32"))]
  use super::load_config_concurrent;
  use super::load_config_local;
  use super::merge_unique_documents;
  use super::root_contains_document;
  use crate::LocalFuture;
  use crate::config::InitConfig;
  use crate::config::LspConfig;
  use crate::config::LspConfigError;
  use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;

  /// Native failures constructing inputs before a world scenario starts.
  #[derive(Debug, Error)]
  enum WorldFixtureError {
    /// HTTP capability construction failed.
    #[error(transparent)]
    Transport(#[from] Box<ResultFailure<TransportError>>),
    /// World or document construction failed.
    #[error(transparent)]
    World(#[from] Box<ResultFailure<WorldError>>),
    /// A fixture URL was invalid.
    #[error(transparent)]
    Url(#[from] ResultFailure<ParseError>),
    /// The default client configuration was invalid.
    #[error(transparent)]
    Config(#[from] ResultFailure<LspConfigError>),
  }

  /// Local schema transport used by world owner tests.
  type LocalTestTransport = LocalSchemaTransport<TestEnvironment>;
  /// Local workspace state used by topology preparation tests.
  type LocalTestWorkspace = WorkspaceState<LocalTestTransport>;
  /// Local world state used by topology behavior tests.
  type LocalTestWorld = WorldState<TestEnvironment, LocalTestTransport>;
  /// Complete local and concurrent configuration-loader results.
  type ConfigurationPair = [Result<Config, WorldError>; 2];
  /// Complete effects of a workspace configuration application.
  type ConfigurationResult = Result<Vec<DidChangeSchemaAssociationParams>, WorldError>;
  /// Project owner, rooted identity, and the complete initialization effects.
  type InitializedProject = (LocalTestWorld, Url, ConfigurationResult);

  /// Parse one fixture URL through the native extraction vocabulary.
  fn url(value: &str) -> Result<Url, ResultFailure<ParseError>> {
    ensure_ok(Url::parse(value), "the world-state fixture URL must parse")
  }

  /// Construct one local world whose HTTP capability is never exercised by these tests.
  fn local_world() -> Result<LocalTestWorld, WorldFixtureError> {
    let environment = TestEnvironment::default();
    let client = ensure_ok(local_http_client(), "the local schema client must construct").map_err(Box::new)?;
    let transport = LocalSchemaTransport::new(environment.clone(), client);
    Ok(
      ensure_ok(
        WorldState::with_transport(environment, transport),
        "the local world fixture must construct",
      )
      .map_err(Box::new)?,
    )
  }

  /// Construct one default configuration whose prepared file rule selects a named fixture.
  fn default_config(include: &str) -> Config {
    Config {
      include: Some(vec![include.to_owned()]),
      ..Config::default()
    }
  }

  /// Construct one global LSP configuration that avoids remote schemas.
  fn schema_disabled_configuration(semantic_tokens: bool) -> Value {
    json!({ "schema": { "enabled": false, "catalogs": [] }, "syntax": { "semanticTokens": semantic_tokens } })
  }

  /// Construct one client-owned schema association.
  fn manual_association(schema_url: Url, association_priority: usize) -> SchemaAssociation {
    SchemaAssociation {
      url:      schema_url,
      meta:     json!({ "source": source::MANUAL }),
      priority: association_priority,
    }
  }

  /// Replace the one configured LSP association used by a workspace test.
  fn set_lsp_association(config: &mut LspConfig, pattern: &str, schema: &str) {
    config.schema.associations.clear();
    drop(config.schema.associations.insert(pattern.to_owned(), schema.to_owned()));
  }

  /// A complete configuration application and its immediately visible committed state.
  #[derive(Debug)]
  struct AssociationObservation {
    /// Native notifications or configuration failure.
    result:    ConfigurationResult,
    /// Complete selected association at this boundary.
    selected:  Option<SchemaAssociation>,
    /// Configuration and schema revisions at this boundary.
    revisions: (Revision, Revision),
  }

  /// Capture committed association state before another configuration attempt can change it.
  fn association_observation<T: SchemaTransport>(
    workspace: &WorkspaceState<T>,
    document: &Url,
    result: ConfigurationResult,
  ) -> AssociationObservation {
    AssociationObservation {
      result,
      selected: workspace.schemas.associations().association_for(document),
      revisions: (workspace.config_revision, workspace.schema_revision),
    }
  }

  /// Exercise the complete configured-association transaction for each execution family.
  macro_rules! workspace_association_contract {
    ($name:ident, $transport:ident, $apply_configuration:ident) => {
      #[test]
      fn $name() -> Result<(), impl Debug> {
        let observations = block_on(async {
          let environment = TestEnvironment::default();
          let transport = $transport::new(environment.clone(), ensure_ok(local_http_client(), "the association client must construct").map_err(Box::new)?);
          let root = WorkspaceRoot::Rooted(url("file:///workspace/")?);
          let document = url("file:///workspace/document.toml")?;
          let committed_schema = url("file:///workspace/schema.json")?;
          let recovered_schema = url("file:///workspace/recovered.json")?;
          let parsed = ensure_ok(DocumentState::parse("value = 1\n"), "the association document must parse").map_err(Box::new)?;
          let mut workspace = ensure_ok(WorkspaceState::new(root, transport.clone()), "the rooted association workspace must construct").map_err(Box::new)?;
          let mut detached = ensure_ok(WorkspaceState::new(WorkspaceRoot::Detached, transport), "the detached association workspace must construct").map_err(Box::new)?;
          workspace.config.schema.catalogs.clear();
          detached.config.schema.catalogs.clear();
          let previous = workspace.documents.insert(document.clone(), parsed);
          set_lsp_association(&mut workspace.config, r".*/document\.toml$", "./schema.json");
          let committed = workspace.$apply_configuration(&environment, Config::default(), Revision(1)).await;
          let committed = association_observation(&workspace, &document, committed);
          set_lsp_association(&mut workspace.config, "[", "https://example.com/rejected-pattern.json");
          let rejected_pattern = workspace.$apply_configuration(&environment, Config::default(), Revision(2)).await;
          let rejected_pattern = association_observation(&workspace, &document, rejected_pattern);
          set_lsp_association(&mut workspace.config, r".*/document\.toml$", "::");
          let rejected_url = workspace.$apply_configuration(&environment, Config::default(), Revision(3)).await;
          let rejected_url = association_observation(&workspace, &document, rejected_url);
          workspace.config.schema.enabled = false;
          set_lsp_association(&mut workspace.config, r".*/document\.toml$", "./disabled.json");
          let disabled = workspace.$apply_configuration(&environment, Config::default(), Revision(4)).await;
          let disabled = association_observation(&workspace, &document, disabled);
          workspace.config.schema.enabled = true;
          set_lsp_association(&mut workspace.config, r".*/document\.toml$", "./recovered.json");
          let recovered = workspace.$apply_configuration(&environment, Config::default(), Revision(5)).await;
          let recovered = association_observation(&workspace, &document, recovered);
          set_lsp_association(&mut detached.config, r".*\.toml$", "./schema.json");
          let detached_result = detached.$apply_configuration(&environment, Config::default(), Revision(1)).await;
          let detached_result = association_observation(&detached, &document, detached_result);
          Ok::<_, WorldFixtureError>((workspace, detached, document, committed_schema, recovered_schema, previous,
            [committed, rejected_pattern, rejected_url, disabled, recovered, detached_result]))
        });
        ensure_that(observations, "association commit, rejection, disablement, recovery, and detached rejection must retain their complete transaction outcomes", |result| {
          let Ok((_, _, ref document, ref committed_schema, ref recovered_schema, ref previous, ref observations)) = *result else { return false; };
          let [ref committed, ref rejected_pattern, ref rejected_url, ref disabled, ref recovered, ref detached] = *observations;
          previous.is_none()
            && committed.result.as_ref().is_ok_and(|notifications| notifications.iter().any(|notification| notification.document_uri == *document && notification.schema_uri.as_ref() == Some(committed_schema)))
            && committed.selected.as_ref().is_some_and(|selected| selected.url == *committed_schema && selected.priority == priority::LSP_CONFIG && selected.meta.get("source") == Some(&json!(source::LSP_CONFIG)))
            && committed.revisions == (Revision(1), Revision(1))
            && matches!(rejected_pattern.result, Err(WorldError::AssociationPattern { ref pattern, .. }) if pattern == "[")
            && rejected_pattern.selected.as_ref().is_some_and(|selected| selected.url == *committed_schema)
            && rejected_pattern.revisions == (Revision(1), Revision(1))
            && matches!(rejected_url.result, Err(WorldError::AssociationUrl { ref source_value, .. }) if source_value == "::")
            && rejected_url.selected.as_ref().is_some_and(|selected| selected.url == *committed_schema)
            && rejected_url.revisions == (Revision(1), Revision(1))
            && disabled.result.as_ref().is_ok_and(|notifications| notifications.iter().any(|notification| notification.document_uri == *document && notification.schema_uri.is_none()))
            && disabled.selected.is_none() && disabled.revisions == (Revision(4), Revision(4))
            && recovered.result.is_ok() && recovered.selected.as_ref().is_some_and(|selected| selected.url == *recovered_schema)
            && recovered.revisions == (Revision(5), Revision(5))
            && matches!(detached.result, Err(WorldError::DetachedRelativePath { ref path }) if path == Path::new("./schema.json"))
            && detached.selected.is_none() && detached.revisions == (Revision::INITIAL, Revision::INITIAL)
        }).map(drop).map_err(Box::new)
      }
    };
  }

  workspace_association_contract!(
    local_workspace_lsp_associations_validate_commit_disable_and_recover,
    LocalSchemaTransport,
    apply_configuration_local
  );
  #[cfg(not(target_arch = "wasm32"))]
  workspace_association_contract!(
    concurrent_workspace_lsp_associations_validate_commit_disable_and_recover,
    ConcurrentSchemaTransport,
    apply_configuration_concurrent
  );

  /// Load one configuration through both execution families, preserving both native outcomes.
  fn load_config_pair<'fixture>(
    environment: &'fixture TestEnvironment,
    root: &'fixture WorkspaceRoot,
    lsp_config: &'fixture LspConfig,
    default: &'fixture Config,
  ) -> LocalFuture<'fixture, ConfigurationPair> {
    Box::pin(async move {
      [
        load_config_local(environment, root, lsp_config, default).await,
        load_config_concurrent(environment, root, lsp_config, default).await,
      ]
    })
  }

  #[test]
  fn configuration_loaders_share_rooted_discovery_and_explicit_path_semantics() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let environment = TestEnvironment::default();
      environment.insert_file("/workspace/taplo.toml", b"include = [\"discovered.toml\"]\n".to_vec());
      environment.insert_file("/workspace/config/project.toml", b"include = [\"relative.toml\"]\n".to_vec());
      environment.insert_file("/configs/absolute.toml", b"include = [\"absolute.toml\"]\n".to_vec());
      let root = WorkspaceRoot::Rooted(url("file:///workspace")?);
      let default = default_config("default.toml");
      let discovered_config = ensure_ok(LspConfig::new(), "the discovery configuration must construct")?;
      let mut relative_config = discovered_config.clone();
      relative_config.taplo.config_file.path = Some(PathBuf::from("config/project.toml"));
      let mut absolute_config = discovered_config.clone();
      absolute_config.taplo.config_file.path = Some(PathBuf::from("/configs/absolute.toml"));
      let discovered = load_config_pair(&environment, &root, &discovered_config, &default).await;
      let discovered_bases = environment.discovery_bases();
      let relative = load_config_pair(&environment, &root, &relative_config, &default).await;
      let absolute = load_config_pair(&environment, &root, &absolute_config, &default).await;
      let final_bases = environment.discovery_bases();
      Ok::<_, WorldFixtureError>((environment, discovered, relative, absolute, discovered_bases, final_bases))
    });
    let discovery_matches = |loaded: &Result<Config, WorldError>| {
      let Ok(ref config) = *loaded else {
        return false;
      };
      config.is_included(Path::new("/workspace/discovered.toml")) && !config.is_included(Path::new("/workspace/default.toml"))
    };
    ensure_that(
      observations,
      "local and concurrent loaders must preserve rooted discovery, replacement, and explicit-path semantics",
      |result| {
        let Ok((_, ref discovered, ref relative, ref absolute, ref discovered_bases, ref final_bases)) = *result else {
          return false;
        };
        discovered.iter().all(discovery_matches)
          && relative.iter().all(|loaded| {
            loaded
              .as_ref()
              .is_ok_and(|config| config.is_included(Path::new("/workspace/relative.toml")))
          })
          && absolute.iter().all(|loaded| {
            loaded
              .as_ref()
              .is_ok_and(|config| config.is_included(Path::new("/workspace/absolute.toml")))
          })
          && *discovered_bases == [PathBuf::from("/workspace"), PathBuf::from("/workspace")]
          && final_bases == discovered_bases
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn configuration_loaders_share_disabled_and_detached_default_behavior() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let environment = TestEnvironment::default();
      environment.set_read_failure(true);
      let default = default_config("default.toml");
      let mut disabled = ensure_ok(LspConfig::new(), "the disabled configuration must construct")?;
      let detached_default = disabled.clone();
      disabled.taplo.config_file.enabled = false;
      disabled.taplo.config_file.path = Some(PathBuf::from("/missing/config.toml"));
      let disabled_results = load_config_pair(&environment, &WorkspaceRoot::Detached, &disabled, &default).await;
      let default_results = load_config_pair(&environment, &WorkspaceRoot::Detached, &detached_default, &default).await;
      let discovery = environment.discovery_bases();
      Ok::<_, WorldFixtureError>((environment, disabled_results, default_results, discovery))
    });
    ensure_that(
      observations,
      "disabled and detached loading must preserve defaults without reading or discovery",
      |result| {
        let Ok((_, ref disabled, ref default, ref discovery)) = *result else {
          return false;
        };
        disabled.iter().chain(default).all(|loaded| {
          loaded
            .as_ref()
            .is_ok_and(|config| config.is_included(Path::new("/workspace/default.toml")))
        }) && discovery.is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn configuration_loaders_reject_invalid_host_context_and_recover_after_read_failure() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let environment = TestEnvironment::default();
      let missing_cwd = TestEnvironment::default();
      missing_cwd.set_cwd(None);
      let default = default_config("default.toml");
      let implicit = ensure_ok(LspConfig::new(), "the implicit configuration must construct")?;
      let mut relative = implicit.clone();
      relative.taplo.config_file.path = Some(PathBuf::from("relative.toml"));
      let mut explicit = implicit.clone();
      explicit.taplo.config_file.path = Some(PathBuf::from("/configs/recovery.toml"));
      let root = WorkspaceRoot::Rooted(url("file:///workspace")?);
      environment.insert_file("/configs/recovery.toml", b"include = [\"recovered.toml\"]\n".to_vec());
      let relative_results = load_config_pair(&environment, &WorkspaceRoot::Detached, &relative, &default).await;
      let cwd_results = load_config_pair(&missing_cwd, &WorkspaceRoot::Detached, &implicit, &default).await;
      environment.set_read_failure(true);
      let read_results = load_config_pair(&environment, &root, &explicit, &default).await;
      environment.set_read_failure(false);
      let recovered = load_config_pair(&environment, &root, &explicit, &default).await;
      Ok::<_, WorldFixtureError>((environment, missing_cwd, relative_results, cwd_results, read_results, recovered))
    });
    ensure_that(
      observations,
      "both configuration loaders must retain host failures and recover when reads return",
      |result| {
        let Ok((_, _, ref relative, ref cwd, ref read, ref recovered)) = *result else {
          return false;
        };
        relative
          .iter()
          .all(|loaded| matches!(*loaded, Err(WorldError::DetachedRelativePath { .. })))
          && cwd
            .iter()
            .all(|loaded| matches!(*loaded, Err(WorldError::MissingCurrentDirectory)))
          && read
            .iter()
            .all(|loaded| matches!(*loaded, Err(WorldError::Environment(EnvironmentError::Io { .. }))))
          && recovered.iter().all(|loaded| {
            loaded
              .as_ref()
              .is_ok_and(|config| config.is_included(Path::new("/workspace/recovered.toml")))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn configuration_loaders_preserve_absence_utf8_and_toml_failure_boundaries() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let environment = TestEnvironment::default();
      let root = WorkspaceRoot::Rooted(url("file:///workspace")?);
      let default = default_config("default.toml");
      let implicit = ensure_ok(LspConfig::new(), "the absent-discovery configuration must construct")?;
      let mut invalid_utf8 = implicit.clone();
      invalid_utf8.taplo.config_file.path = Some(PathBuf::from("/configs/non-utf8.toml"));
      let mut invalid_toml = implicit.clone();
      invalid_toml.taplo.config_file.path = Some(PathBuf::from("/configs/invalid.toml"));
      let absent = load_config_pair(&environment, &root, &implicit, &default).await;
      let discovery = environment.discovery_bases();
      environment.insert_file("/configs/non-utf8.toml", vec![0xff]);
      let utf8_results = load_config_pair(&environment, &root, &invalid_utf8, &default).await;
      environment.insert_file("/configs/invalid.toml", b"include = [\n".to_vec());
      let toml_results = load_config_pair(&environment, &root, &invalid_toml, &default).await;
      Ok::<_, WorldFixtureError>((environment, absent, discovery, utf8_results, toml_results))
    });
    ensure_that(
      observations,
      "configuration absence and malformed contents must preserve distinct native outcomes",
      |result| {
        let Ok((_, ref absent, ref discovery, ref utf8, ref toml)) = *result else {
          return false;
        };
        absent.iter().all(|loaded| {
          loaded
            .as_ref()
            .is_ok_and(|config| config.is_included(Path::new("/workspace/default.toml")))
        }) && *discovery == [PathBuf::from("/workspace"), PathBuf::from("/workspace")]
          && utf8
            .iter()
            .all(|loaded| matches!(*loaded, Err(WorldError::ConfigUtf8 { ref path, .. }) if path == Path::new("/configs/non-utf8.toml")))
          && toml
            .iter()
            .all(|loaded| matches!(*loaded, Err(WorldError::ConfigToml { ref path, .. }) if path == Path::new("/configs/invalid.toml")))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Require a real concurrently published value to remain transferable and shareable.
  #[cfg(not(target_arch = "wasm32"))]
  fn require_send_sync<T: Send + Sync>(value: T) -> T {
    value
  }

  #[cfg(not(target_arch = "wasm32"))]
  #[test]
  fn concurrent_document_snapshots_are_send_and_sync() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let environment = TestEnvironment::default();
      let document = url("file:///workspace/document.toml")?;
      let client = ensure_ok(local_http_client(), "the concurrent schema client must construct").map_err(Box::new)?;
      let transport = ConcurrentSchemaTransport::new(environment.clone(), client);
      let world = require_send_sync(
        ensure_ok(
          WorldState::with_transport(environment, transport),
          "the concurrent world must construct",
        )
        .map_err(Box::new)?,
      );
      let installed = world.replace_document_concurrent(&document, "value = 1\n").await;
      let snapshot = require_send_sync(world.document_snapshot_concurrent(&document).await);
      Ok::<_, WorldFixtureError>((world, installed, snapshot))
    });
    ensure_that(
      observations,
      "the concurrent world and its published document snapshot must be transferable and shareable",
      |result| result.as_ref().is_ok_and(|observed| observed.1.is_ok() && observed.2.is_some()),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn workspace_ownership_respects_url_identity_and_path_boundaries() -> Result<(), impl Debug> {
    let observations = (|| {
      let root = url("file:///workspace")?;
      let nested = url("file:///workspace/nested")?;
      let document = url("file:///workspace/nested/file.toml")?;
      let cases = [
        (root.clone(), document.clone(), true),
        (root.clone(), root.clone(), true),
        (url("file:///")?, document.clone(), true),
        (root.clone(), url("file:///workspace-other/file.toml")?, false),
        (root.clone(), url("https://example.com/workspace/file.toml")?, false),
        (
          url("https://user@example.com/workspace")?,
          url("https://example.com/workspace/file.toml")?,
          false,
        ),
        (
          url("https://example.com:8443/workspace")?,
          url("https://example.com/workspace/file.toml")?,
          false,
        ),
        (url("mailto:user@example.com")?, url("mailto:other@example.com")?, false),
      ];
      let ownership = cases.map(|(owner, child, expected)| {
        let actual = root_contains_document(&owner, &child);
        (owner, child, actual, expected)
      });
      let deepest = deepest_root([&root, &nested].into_iter(), &document).cloned();
      Ok::<_, WorldFixtureError>((
        WorkspaceRoot::Detached,
        WorkspaceRoot::Rooted(root.clone()),
        root,
        nested,
        document,
        ownership,
        deepest,
      ))
    })();
    ensure_that(
      observations,
      "workspace ownership must respect complete URL identities, segment boundaries, and deepest-root selection",
      |result| {
        let Ok((ref detached, ref rooted, ref expected_root, ref nested, _, ref ownership, ref deepest)) = *result else {
          return false;
        };
        detached.url().is_none()
          && rooted.url() == Some(expected_root)
          && ownership.iter().all(|case| case.2 == case.3)
          && deepest.as_ref() == Some(nested)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn duplicate_roots_and_revision_exhaustion_are_typed_failures() -> Result<(), impl Debug> {
    let observations = (|| {
      let root = url("file:///workspace")?;
      let document = url("file:///workspace/duplicate.toml")?;
      let state = ensure_ok(DocumentState::parse("value = 1\n"), "the duplicate-ownership document must parse").map_err(Box::new)?;
      let world = local_world()?;
      let roots = [root.clone(), root];
      let duplicates = ensure_unique_roots(&roots);
      world.revision.store(u64::MAX, Ordering::SeqCst);
      let revision = world.candidate_revision();
      let mut documents = HashMap::default();
      let installed = insert_unique_document(&mut documents, document.clone(), state.clone());
      let incoming = HashMap::from_iter([(document.clone(), state)]);
      let merged = merge_unique_documents(&mut documents, incoming);
      Ok::<_, WorldFixtureError>((world, roots, duplicates, revision, document, documents, installed, merged))
    })();
    ensure_that(
      observations,
      "duplicate topology/document owners and exhausted revisions must retain their native failures",
      |result| {
        let Ok((_, _, ref duplicates, ref revision, ref document, ref documents, ref installed, ref merged)) = *result else {
          return false;
        };
        matches!(*duplicates, Err(WorldError::DuplicateWorkspaceRoot { .. }))
          && matches!(*revision, Err(WorldError::RevisionExhausted))
          && installed.is_ok()
          && matches!(*merged, Err(WorldError::DuplicateDocumentOwnership { document: ref duplicate }) if duplicate == document)
          && documents.contains_key(document)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Prepare one topology candidate using its loaded configuration with schemas disabled.
  fn prepare_schema_disabled(
    mut preparation: WorkspacePreparation<LocalTestTransport>,
    transaction_environment: TestEnvironment,
  ) -> LocalFuture<'static, Result<LocalTestWorkspace, WorldError>> {
    preparation.seed.config.schema.enabled = false;
    preparation.seed.config.schema.catalogs.clear();
    Box::pin(async move { preparation.prepare_loaded_local(&transaction_environment).await })
  }

  /// Initialize a project world while retaining the native initialization outcome.
  fn initialized_project_world() -> LocalFuture<'static, Result<InitializedProject, WorldFixtureError>> {
    Box::pin(async {
      let world = local_world()?;
      let root = url("file:///workspace/project")?;
      let environment = world.env.clone();
      let initialized = WorldTransaction::new(&world, move |preparation| prepare_schema_disabled(preparation, environment.clone()))
        .initialize_roots_with(Arc::new(InitConfig::default()), vec![root.clone()])
        .await;
      Ok((world, root, initialized))
    })
  }

  #[test]
  fn initialization_commits_atomically_and_rejects_reinitialization() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let (world, root, initialized) = initialized_project_world().await?;
      let roots = world.rooted_workspace_urls().await;
      let debug = format!("{world:?}");
      let topology_guard = world.workspaces.write().await;
      let locked_debug = format!("{world:?}");
      drop(topology_guard);
      let reinitialized = world
        .initialize_roots_local(Arc::new(InitConfig::default()), vec![root.clone()])
        .await;
      let initialized_revision = world.revision.load(Ordering::SeqCst);
      let empty = world.apply_configuration_values_local(None, &[]).await;
      let final_revision = world.revision.load(Ordering::SeqCst);
      Ok::<_, WorldFixtureError>((
        world,
        root,
        initialized,
        roots,
        debug,
        locked_debug,
        reinitialized,
        empty,
        (initialized_revision, final_revision),
      ))
    });
    ensure_that(
      observations,
      "initialization must commit once, keep debug reads nonblocking, and preserve no-op revisions",
      |result| {
        let Ok((_, ref root, ref initialized, ref roots, ref debug, ref locked_debug, ref reinitialized, ref empty, revisions)) = *result
        else {
          return false;
        };
        initialized.is_ok()
          && roots == from_ref(root)
          && debug.contains("revision: 1")
          && debug.contains("rooted_workspaces: 1")
          && debug.contains("open_documents: 0")
          && locked_debug.contains("revision: 1")
          && !locked_debug.contains("rooted_workspaces")
          && !locked_debug.contains("open_documents")
          && matches!(*reinitialized, Err(WorldError::AlreadyInitialized))
          && empty.as_ref().is_ok_and(Vec::is_empty)
          && revisions.0 == revisions.1
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn scoped_configuration_validates_before_atomic_commit() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let missing_root = url("file:///workspace/missing")?;
      let (world, root, initialized) = initialized_project_world().await?;
      let before_revision = world.revision.load(Ordering::SeqCst);
      let invalid_global = Value::String(String::from("invalid"));
      let global_failure = world.apply_configuration_values_local(Some(&invalid_global), &[]).await;
      let invalid_scoped = [(root.clone(), Value::Bool(false))];
      let scoped_failure = world.apply_configuration_values_local(None, &invalid_scoped).await;
      let missing_scoped = [(missing_root.clone(), json!({}))];
      let missing_failure = world.apply_configuration_values_local(None, &missing_scoped).await;
      let rejected_revision = world.revision.load(Ordering::SeqCst);
      let global = json!({ "schema": { "enabled": false, "catalogs": [], "links": true } });
      let scoped = [(root.clone(), json!({ "syntax": { "semanticTokens": false } }))];
      let committed = world.apply_configuration_values_local(Some(&global), &scoped).await;
      let topology = world.workspaces.read().await;
      let rooted = topology.rooted(&root);
      let detached = Arc::clone(&topology.detached);
      drop(topology);
      let rooted_config = match rooted {
        Some(handle) => Some(handle.read().await.config.clone()),
        None => None,
      };
      let detached_config = detached.read().await.config.clone();
      Ok::<_, WorldFixtureError>((
        world,
        root,
        missing_root,
        initialized,
        (global_failure, scoped_failure, missing_failure, before_revision, rejected_revision),
        committed,
        rooted_config,
        detached_config,
      ))
    });
    ensure_that(
      observations,
      "configuration rejection must preserve revision and valid global/scoped updates must commit atomically",
      |result| {
        let Ok((_, ref root, ref missing_root, ref initialized, ref rejected, ref committed, ref rooted, ref detached)) = *result else {
          return false;
        };
        initialized.is_ok()
          && matches!(rejected.0, Err(WorldError::ConfigurationResponse { ref scope, .. }) if scope == "global workspace configuration")
          && matches!(rejected.1, Err(WorldError::ConfigurationResponse { ref scope, .. }) if scope == root.as_str())
          && matches!(rejected.2, Err(WorldError::MissingWorkspace { root: ref missing }) if missing == missing_root)
          && rejected.3 == rejected.4
          && committed.is_ok()
          && rooted
            .as_ref()
            .is_some_and(|config| config.schema.links && !config.syntax.semantic_tokens)
          && detached.schema.links
          && detached.syntax.semantic_tokens
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn configuration_reclassifies_included_and_excluded_open_documents() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let world = local_world()?;
      let included = url("file:///workspace/included.toml")?;
      let excluded = url("file:///workspace/excluded.toml")?;
      world.set_default_config(Arc::new(default_config("included.toml")));
      let configuration = schema_disabled_configuration(true);
      let initial = world.apply_configuration_values_local(Some(&configuration), &[]).await;
      let included_update = world.replace_document(&included, "value = 1\n").await;
      let excluded_update = world.replace_document(&excluded, "value = 2\n").await;
      let before = [
        world.document_snapshot(&included).await,
        world.document_snapshot(&excluded).await,
      ];
      world.set_default_config(Arc::new(default_config("excluded.toml")));
      let replacement = world.apply_configuration_values_local(Some(&configuration), &[]).await;
      let after = [
        world.document_snapshot(&included).await,
        world.document_snapshot(&excluded).await,
      ];
      let dispositions = world.open_document_dispositions().await;
      Ok::<_, WorldFixtureError>((
        world, included, excluded, initial, included_update, excluded_update, before, replacement, after, dispositions,
      ))
    });
    ensure_that(
      observations,
      "configuration must atomically reclassify every retained document and preserve ordered dispositions",
      |result| {
        let Ok((
          _,
          ref included,
          ref excluded,
          ref initial,
          ref included_update,
          ref excluded_update,
          ref before,
          ref replacement,
          ref after,
          ref dispositions,
        )) = *result
        else {
          return false;
        };
        initial.is_ok()
          && replacement.is_ok()
          && included_update
            .as_ref()
            .is_ok_and(|update| update.disposition == DocumentDisposition::Included)
          && excluded_update
            .as_ref()
            .is_ok_and(|update| update.disposition == DocumentDisposition::Excluded)
          && matches!(*before, [Some(_), None])
          && matches!(*after, [None, Some(_)])
          && *dispositions
            == [
              (excluded.clone(), DocumentDisposition::Included),
              (included.clone(), DocumentDisposition::Excluded),
            ]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_replacement_and_close_transactions_are_idempotent_at_their_boundaries() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let world = local_world()?;
      let document = url("file:///workspace/document.toml")?;
      let unknown = url("file:///workspace/unknown.toml")?;
      let initial_revision = world.revision.load(Ordering::SeqCst);
      let unknown_close = world.close_document(&unknown).await;
      let unknown_revision = world.revision.load(Ordering::SeqCst);
      let first = world.replace_document(&document, "value = 1\n").await;
      let first_revision = world.revision.load(Ordering::SeqCst);
      let second = world.replace_document(&document, "value = 2\n").await;
      let second_revision = world.revision.load(Ordering::SeqCst);
      let snapshot = world.document_snapshot(&document).await;
      let semantic = snapshot.as_ref().map(|captured| serde_json::to_value(&captured.document.dom));
      let closed = world.close_document(&document).await;
      let closed_revision = world.revision.load(Ordering::SeqCst);
      let after_close = world.document_snapshot(&document).await;
      let repeated = world.close_document(&document).await;
      let repeated_revision = world.revision.load(Ordering::SeqCst);
      Ok::<_, WorldFixtureError>((
        world,
        (unknown_close, initial_revision, unknown_revision),
        (first, first_revision, second, second_revision, snapshot, semantic),
        (closed, closed_revision, after_close, repeated, repeated_revision),
      ))
    });
    ensure_that(
      observations,
      "document replacement and close must retain effects, semantic snapshots, and idempotent revision boundaries",
      |result| {
        let Ok((_, ref unknown, ref replacements, ref closes)) = *result else {
          return false;
        };
        unknown.0.as_ref().is_ok_and(Vec::is_empty)
          && unknown.1 == unknown.2
          && replacements
            .0
            .as_ref()
            .is_ok_and(|update| update.disposition == DocumentDisposition::Included)
          && replacements
            .2
            .as_ref()
            .is_ok_and(|update| update.disposition == DocumentDisposition::Included)
          && replacements.3 > replacements.1
          && replacements.4.is_some()
          && replacements
            .5
            .as_ref()
            .is_some_and(|semantic| semantic.as_ref().is_ok_and(|value| *value == json!({ "value": 2 })))
          && closes.0.is_ok()
          && closes.2.is_none()
          && closes.3.as_ref().is_ok_and(Vec::is_empty)
          && closes.1 == closes.4
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// A schema-independent topology fixture retaining both document installation outcomes.
  #[derive(Debug)]
  struct TopologyFixture {
    /// World whose topology is under test.
    world:             LocalTestWorld,
    /// Valid workspace root used by the transaction.
    root:              Url,
    /// Document beneath the future root.
    rooted_document:   Url,
    /// Document outside every configured root.
    detached_document: Url,
    /// Native results from opening both documents.
    installations:     [Result<DocumentUpdate, WorldError>; 2],
  }

  /// Build the topology fixture without contacting schema catalogs or discarding installation
  /// effects.
  fn topology_fixture() -> LocalFuture<'static, Result<TopologyFixture, WorldFixtureError>> {
    Box::pin(async {
      let world = local_world()?;
      let root = url("file:///workspace/project")?;
      let rooted_document = url("file:///workspace/project/document.toml")?;
      let detached_document = url("file:///outside/document.toml")?;
      let topology = world.workspaces.read().await;
      let mut detached = topology.detached.write().await;
      detached.config.schema.enabled = false;
      detached.config.schema.catalogs.clear();
      drop(detached);
      drop(topology);
      let installations = [
        world.replace_document(&rooted_document, "owner = \"root\"\n").await,
        world.replace_document(&detached_document, "owner = \"detached\"\n").await,
      ];
      Ok(TopologyFixture {
        world,
        root,
        rooted_document,
        detached_document,
        installations,
      })
    })
  }

  #[test]
  fn topology_transactions_redistribute_documents_and_preserve_no_ops() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let unknown = url("file:///workspace/unknown")?;
      let fixture = topology_fixture().await?;
      let add_environment = fixture.world.env.clone();
      let added = WorldTransaction::new(&fixture.world, move |preparation| {
        prepare_schema_disabled(preparation, add_environment.clone())
      })
      .change_roots_with(&[], from_ref(&fixture.root))
      .await;
      let added_roots = fixture.world.rooted_workspace_urls().await;
      let snapshots = [
        fixture.world.document_snapshot(&fixture.rooted_document).await,
        fixture.world.document_snapshot(&fixture.detached_document).await,
      ];
      let committed_revision = fixture.world.revision.load(Ordering::SeqCst);
      let repeat_environment = fixture.world.env.clone();
      let repeated_add = WorldTransaction::new(&fixture.world, move |preparation| {
        prepare_schema_disabled(preparation, repeat_environment.clone())
      })
      .change_roots_with(&[], from_ref(&fixture.root))
      .await;
      let unknown_remove = fixture.world.change_roots_local(from_ref(&unknown), &[]).await;
      let no_op_revision = fixture.world.revision.load(Ordering::SeqCst);
      let remove_environment = fixture.world.env.clone();
      let removed = WorldTransaction::new(&fixture.world, move |preparation| {
        prepare_schema_disabled(preparation, remove_environment.clone())
      })
      .change_roots_with(from_ref(&fixture.root), &[])
      .await;
      let final_roots = fixture.world.rooted_workspace_urls().await;
      Ok::<_, WorldFixtureError>((
        fixture,
        added,
        added_roots,
        snapshots,
        (repeated_add, unknown_remove, committed_revision, no_op_revision),
        removed,
        final_roots,
      ))
    });
    ensure_that(
      observations,
      "topology changes must redistribute both documents while repeated changes preserve effects and revision",
      |result| {
        let Ok((ref fixture, ref added, ref added_roots, ref snapshots, ref no_ops, ref removed, ref final_roots)) = *result else {
          return false;
        };
        let expected_documents = [fixture.detached_document.clone(), fixture.rooted_document.clone()];
        fixture.installations.iter().all(Result::is_ok)
          && added_roots == from_ref(&fixture.root)
          && added.as_ref().is_ok_and(|update| update.documents == expected_documents)
          && snapshots.iter().all(Option::is_some)
          && no_ops
            .0
            .as_ref()
            .is_ok_and(|update| update.documents.is_empty() && update.notifications.is_empty())
          && no_ops
            .1
            .as_ref()
            .is_ok_and(|update| update.documents.is_empty() && update.notifications.is_empty())
          && no_ops.2 == no_ops.3
          && removed.as_ref().is_ok_and(|update| update.documents == expected_documents)
          && final_roots.is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn added_roots_inherit_global_configuration_while_retained_roots_preserve_scoped_overrides() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let world = local_world()?;
      let first_root = url("file:///workspace/first")?;
      let second_root = url("file:///workspace/second")?;
      let global = schema_disabled_configuration(false);
      let configured = world.apply_configuration_values_local(Some(&global), &[]).await;
      let first_added = world.change_roots_local(&[], from_ref(&first_root)).await;
      let scoped = [(first_root.clone(), json!({ "syntax": { "semanticTokens": true } }))];
      let scoped_result = world.apply_configuration_values_local(None, &scoped).await;
      let second_added = world.change_roots_local(&[], from_ref(&second_root)).await;
      let topology = world.workspaces.read().await;
      let detached = Arc::clone(&topology.detached);
      let first = topology.rooted(&first_root);
      let second = topology.rooted(&second_root);
      drop(topology);
      let detached_config = detached.read().await.config.clone();
      let first_config = match first {
        Some(handle) => Some(handle.read().await.config.clone()),
        None => None,
      };
      let second_config = match second {
        Some(handle) => Some(handle.read().await.config.clone()),
        None => None,
      };
      Ok::<_, WorldFixtureError>((
        world, configured, first_added, scoped_result, second_added, detached_config, first_config, second_config,
      ))
    });
    ensure_that(
      observations,
      "new roots must inherit global configuration while retained roots preserve their scoped overlay",
      |result| {
        let Ok((_, ref configured, ref first_added, ref scoped, ref second_added, ref detached, ref first, ref second)) = *result else {
          return false;
        };
        configured.is_ok()
          && first_added.is_ok()
          && scoped.is_ok()
          && second_added.is_ok()
          && !detached.schema.enabled
          && !detached.syntax.semantic_tokens
          && first
            .as_ref()
            .is_some_and(|config| !config.schema.enabled && config.syntax.semantic_tokens)
          && second
            .as_ref()
            .is_some_and(|config| !config.schema.enabled && !config.syntax.semantic_tokens)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn topology_transactions_are_failure_atomic_and_recover() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let invalid_root = url("https://example.com/workspace")?;
      let fixture = topology_fixture().await?;
      let before = fixture.world.revision.load(Ordering::SeqCst);
      let rejected = fixture.world.change_roots_local(&[], from_ref(&invalid_root)).await;
      let rejected_roots = fixture.world.rooted_workspace_urls().await;
      let rejected_revision = fixture.world.revision.load(Ordering::SeqCst);
      let snapshots = [
        fixture.world.document_snapshot(&fixture.rooted_document).await,
        fixture.world.document_snapshot(&fixture.detached_document).await,
      ];
      let recovery_environment = fixture.world.env.clone();
      let recovered = WorldTransaction::new(&fixture.world, move |preparation| {
        prepare_schema_disabled(preparation, recovery_environment.clone())
      })
      .change_roots_with(&[], from_ref(&fixture.root))
      .await;
      let final_roots = fixture.world.rooted_workspace_urls().await;
      Ok::<_, WorldFixtureError>((
        fixture, rejected, rejected_roots, before, rejected_revision, snapshots, recovered, final_roots,
      ))
    });
    ensure_that(
      observations,
      "invalid topology preparation must retain every document and revision before recovery commits",
      |result| {
        let Ok((ref fixture, ref rejected, ref rejected_roots, before, rejected_revision, ref snapshots, ref recovered, ref final_roots)) =
          *result
        else {
          return false;
        };
        fixture.installations.iter().all(Result::is_ok)
          && matches!(*rejected, Err(WorldError::InvalidWorkspaceRoot { .. }))
          && rejected_roots.is_empty()
          && before == rejected_revision
          && snapshots.iter().all(Option::is_some)
          && recovered.is_ok()
          && final_roots == from_ref(&fixture.root)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn embedding_cache_default_yields_to_explicit_client_configuration() -> Result<(), impl Debug> {
    let observations = (|| {
      let world = local_world()?;
      let host_cache = PathBuf::from("/host/cache");
      let client_cache = PathBuf::from("/client/cache");
      world.set_default_cache_path(Some(host_cache.clone()));
      let inherited = world.effective_init_config(Arc::new(InitConfig {
        cache_path:            None,
        configuration_section: "client-section".into(),
      }));
      let explicit = world.effective_init_config(Arc::new(InitConfig {
        cache_path:            Some(client_cache.clone()),
        configuration_section: "explicit-section".into(),
      }));
      Ok::<_, WorldFixtureError>((world, host_cache, client_cache, inherited, explicit))
    })();
    ensure_that(
      observations,
      "client cache selection must override host defaults while preserving the configuration section",
      |result| {
        let Ok((_, ref host_cache, ref client_cache, ref inherited, ref explicit)) = *result else {
          return false;
        };
        inherited.cache_path.as_ref() == Some(host_cache)
          && inherited.configuration_section == "client-section"
          && explicit.cache_path.as_ref() == Some(client_cache)
          && explicit.configuration_section == "explicit-section"
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn cache_configuration_propagates_clock_failures_atomically_and_recovers() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let environment = TestEnvironment::default();
      let client = ensure_ok(local_http_client(), "the cache-policy schema client must construct").map_err(Box::new)?;
      let transport = LocalSchemaTransport::new(environment.clone(), client);
      let mut workspace = ensure_ok(
        WorkspaceState::new(WorkspaceRoot::Detached, transport),
        "the cache-policy workspace must construct",
      )
      .map_err(Box::new)?;
      workspace.config.schema.enabled = false;
      workspace.config.schema.catalogs.clear();
      let committed = workspace
        .apply_configuration_local(&environment, Config::default(), Revision(1))
        .await;
      let committed_revisions = (workspace.config_revision, workspace.schema_revision);
      environment.set_clock_available(false);
      let rejected = workspace
        .apply_configuration_local(&environment, Config::default(), Revision(2))
        .await;
      let rejected_revisions = (workspace.config_revision, workspace.schema_revision);
      environment.set_clock_available(true);
      let recovered = workspace
        .apply_configuration_local(&environment, Config::default(), Revision(3))
        .await;
      let recovered_revisions = (workspace.config_revision, workspace.schema_revision);
      Ok::<_, WorldFixtureError>((
        workspace, environment, committed, committed_revisions, rejected, rejected_revisions, recovered, recovered_revisions,
      ))
    });
    ensure_that(
      observations,
      "clock failure must preserve its world/cache/transport/environment sources and committed revisions before recovery",
      |result| {
        let Ok((_, _, ref committed, committed_revisions, ref rejected, rejected_revisions, ref recovered, recovered_revisions)) = *result
        else {
          return false;
        };
        committed.is_ok()
          && committed_revisions == (Revision(1), Revision(1))
          && matches!(
            *rejected,
            Err(WorldError::Cache(CacheError::Transport(TransportError::Environment(
              EnvironmentError::MissingCallback {
                name: "now"
              }
            ))))
          )
          && rejected_revisions == committed_revisions
          && recovered.is_ok()
          && recovered_revisions == (Revision(3), Revision(3))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn manual_global_associations_select_by_priority_and_reject_invalid_patterns() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let world = local_world()?;
      let document = url("file:///workspace/document.toml")?;
      let glob_schema = url("https://example.com/glob.json")?;
      let regex_schema = url("https://example.com/regex.json")?;
      let invalid_regex_schema = url("https://example.com/invalid-regex.json")?;
      let invalid_glob_schema = url("https://example.com/invalid-glob.json")?;
      let installed = world.replace_document(&document, "value = 1\n").await;
      let glob = world
        .associate_schema(
          ManualAssociationRule::Glob(String::from("**/*.toml")),
          manual_association(glob_schema.clone(), priority::LSP_CONFIG),
        )
        .await;
      let selected_glob = world.associated_schema(&document).await;
      let regex = world
        .associate_schema(
          ManualAssociationRule::Regex(String::from(r".*/document\.toml$")),
          manual_association(regex_schema.clone(), priority::DIRECTIVE),
        )
        .await;
      let selected_regex = world.associated_schema(&document).await;
      let listed = world.list_schema_associations(&document).await;
      let before = world.revision.load(Ordering::SeqCst);
      let rejected_regex = world
        .associate_schema(
          ManualAssociationRule::Regex(String::from("[")),
          manual_association(invalid_regex_schema, priority::MAX),
        )
        .await;
      let rejected_glob = world
        .associate_schema(
          ManualAssociationRule::Glob(String::from("[")),
          manual_association(invalid_glob_schema, priority::MAX),
        )
        .await;
      let after = world.revision.load(Ordering::SeqCst);
      Ok::<_, WorldFixtureError>((
        world,
        installed,
        (glob_schema, glob, selected_glob),
        (regex_schema, regex, selected_regex),
        listed,
        (before, rejected_regex, rejected_glob, after),
      ))
    });
    ensure_that(
      observations,
      "manual global associations must retain both rules, select by priority, and reject malformed rules without revision changes",
      |result| {
        let Ok((_, ref installed, ref glob, ref regex, ref listed, ref rejected)) = *result else {
          return false;
        };
        installed.is_ok()
          && glob
            .1
            .as_ref()
            .is_ok_and(|update| update.diagnostic_document.is_none() && !update.notifications.is_empty())
          && glob.2.as_ref().is_some_and(|selected| selected.url == glob.0)
          && regex.1.is_ok()
          && regex.2.as_ref().is_some_and(|selected| selected.url == regex.0)
          && listed.iter().any(|association| association.url == glob.0)
          && listed.iter().any(|association| association.url == regex.0)
          && matches!(rejected.1, Err(WorldError::ManualAssociationPattern { ref pattern, .. }) if pattern == "[")
          && matches!(rejected.2, Err(WorldError::AssociationGlob(_)))
          && rejected.0 == rejected.3
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn manual_exact_associations_replace_their_prior_owner_atomically() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let world = local_world()?;
      let document = url("file:///workspace/document.toml")?;
      let glob_schema = url("https://example.com/glob.json")?;
      let exact_schema = url("https://example.com/exact.json")?;
      let replacement_schema = url("https://example.com/replacement.json")?;
      let installed = world.replace_document(&document, "value = 1\n").await;
      let glob = world
        .associate_schema(
          ManualAssociationRule::Glob(String::from("**/*.toml")),
          manual_association(glob_schema.clone(), priority::LSP_CONFIG),
        )
        .await;
      let exact = world
        .associate_schema(
          ManualAssociationRule::Url(document.clone()),
          manual_association(exact_schema.clone(), priority::MAX),
        )
        .await;
      let replacement = world
        .associate_schema(
          ManualAssociationRule::Url(document.clone()),
          manual_association(replacement_schema.clone(), priority::MAX),
        )
        .await;
      let selected = world.associated_schema(&document).await;
      let listed = world.list_schema_associations(&document).await;
      Ok::<_, WorldFixtureError>((
        world,
        document,
        installed,
        (glob_schema, glob),
        (exact_schema, exact),
        (replacement_schema, replacement),
        selected,
        listed,
      ))
    });
    ensure_that(
      observations,
      "exact manual associations must replace their owner, identify diagnostics, and remain excluded from global listings",
      |result| {
        let Ok((_, ref document, ref installed, ref glob, ref exact, ref replacement, ref selected, ref listed)) = *result else {
          return false;
        };
        installed.is_ok()
          && glob.1.is_ok()
          && exact
            .1
            .as_ref()
            .is_ok_and(|update| update.diagnostic_document.as_ref() == Some(document))
          && replacement.1.is_ok()
          && selected.as_ref().is_some_and(|association| association.url == replacement.0)
          && listed.iter().any(|association| association.url == glob.0)
          && listed
            .iter()
            .all(|association| association.url != exact.0 && association.url != replacement.0)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Observe snapshot currency without consuming the captured native document state.
  fn snapshot_currency<'capture>(
    world: &'capture LocalTestWorld,
    document: &'capture Url,
    snapshot: Option<&'capture DocumentSnapshot<LocalTestTransport>>,
  ) -> LocalFuture<'capture, Option<bool>> {
    Box::pin(async move {
      match snapshot {
        Some(captured) => Some(world.snapshot_is_current(document, captured).await),
        None => None,
      }
    })
  }

  #[test]
  fn document_schema_and_configuration_mutations_invalidate_old_snapshots() -> Result<(), impl Debug> {
    let observations = block_on(async {
      let world = local_world()?;
      let document = url("file:///workspace/document.toml")?;
      let schema_url = url("https://example.com/schema.json")?;
      let installed = world.replace_document(&document, "value = 1\n").await;
      let original = world.document_snapshot(&document).await;
      let initial_currency = snapshot_currency(&world, &document, original.as_ref()).await;
      let replaced = world.replace_document(&document, "value = 2\n").await;
      let stale_document = snapshot_currency(&world, &document, original.as_ref()).await;
      let after_document = world.document_snapshot(&document).await;
      let associated = world
        .associate_schema(
          ManualAssociationRule::Url(document.clone()),
          manual_association(schema_url, priority::MAX),
        )
        .await;
      let stale_schema = snapshot_currency(&world, &document, after_document.as_ref()).await;
      let after_schema = world.document_snapshot(&document).await;
      let configuration = json!({ "schema": { "enabled": false }, "syntax": { "semanticTokens": false } });
      let configured = world.apply_configuration_values_local(Some(&configuration), &[]).await;
      let stale_configuration = snapshot_currency(&world, &document, after_schema.as_ref()).await;
      let current = world.document_snapshot(&document).await;
      let current_currency = snapshot_currency(&world, &document, current.as_ref()).await;
      let closed = world.close_document(&document).await;
      let stale_closed = snapshot_currency(&world, &document, current.as_ref()).await;
      Ok::<_, WorldFixtureError>((
        world,
        [installed, replaced],
        associated,
        [configured, closed],
        [original, after_document, after_schema, current],
        [
          initial_currency, stale_document, stale_schema, stale_configuration, current_currency, stale_closed,
        ],
      ))
    });
    ensure_that(
      observations,
      "document, schema, configuration, and close mutations must each invalidate previously captured snapshots",
      |result| {
        let Ok((_, ref documents, ref associated, ref configurations, ref snapshots, ref currency)) = *result else {
          return false;
        };
        documents.iter().all(Result::is_ok)
          && associated.is_ok()
          && configurations.iter().all(Result::is_ok)
          && snapshots.iter().all(Option::is_some)
          && *currency == [Some(true), Some(false), Some(false), Some(false), Some(true), Some(false)]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
