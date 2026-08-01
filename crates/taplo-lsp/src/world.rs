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
#[derive(Clone)]
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
  use std::path::Path;
  use std::path::PathBuf;
  use std::slice::from_ref;
  use std::sync::Arc;
  use std::sync::atomic::Ordering;

  use futures::executor::block_on;
  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
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
  use url::Url;

  use super::DocumentSnapshot;
  use super::DocumentState;
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
  use crate::LocalFuture;

  /// Local schema transport used by world owner tests.
  type LocalTestTransport = LocalSchemaTransport<TestEnvironment>;
  /// Local workspace state used by topology preparation tests.
  type LocalTestWorkspace = WorkspaceState<LocalTestTransport>;
  /// Local world state used by topology behavior tests.
  type LocalTestWorld = WorldState<TestEnvironment, LocalTestTransport>;
  use super::root_contains_document;
  use crate::config::InitConfig;
  use crate::config::LspConfig;

  /// Require one concurrently published state type to be transferable and shareable.
  #[cfg(not(target_arch = "wasm32"))]
  fn require_send_sync<T: Send + Sync>() {}

  /// Parse one fixture URL through the panic-free test vocabulary.
  fn url(value: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(value), "the world-state fixture URL must parse")
  }

  /// Construct one local world whose HTTP capability is never exercised by these tests.
  fn local_world() -> Result<WorldState<TestEnvironment, LocalSchemaTransport<TestEnvironment>>, TestFailure> {
    let environment = TestEnvironment::default();
    let client = ensure_ok(local_http_client(), "the local schema client must construct")?;
    let transport = LocalSchemaTransport::new(environment.clone(), client);
    ensure_ok(
      WorldState::with_transport(environment, transport),
      "the local world fixture must construct",
    )
  }

  /// Construct one local world with its standard document already open.
  fn open_document_fixture(context: &'static str) -> LocalFuture<'static, Result<(LocalTestWorld, Url), TestFailure>> {
    Box::pin(async move {
      let world = local_world()?;
      let document = url("file:///workspace/document.toml")?;
      drop(ensure_ok(world.replace_document(&document, "value = 1\n").await, context)?);
      Ok((world, document))
    })
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
    json!({
      "schema": {
        "enabled": false,
        "catalogs": []
      },
      "syntax": {
        "semanticTokens": semantic_tokens
      }
    })
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

  /// Verify that a rejected configuration preserved one committed association transaction.
  fn ensure_committed_association<T: SchemaTransport>(
    workspace: &WorkspaceState<T>,
    document: &Url,
    expected_schema: &Url,
    expected_revision: Revision,
    context: &'static str,
  ) -> Result<(), TestFailure> {
    let selected_schema = workspace
      .schemas
      .associations()
      .association_for(document)
      .map(|association| association.url);
    ensure(
      (selected_schema, workspace.config_revision, workspace.schema_revision)
        == (Some(expected_schema.clone()), expected_revision, expected_revision),
      context,
    )
  }

  /// Generate the configured-association transaction contract for one execution family.
  macro_rules! workspace_association_contract {
    ($name:ident, $transport:ident, $client_context:literal, $apply_configuration:ident) => {
      #[test]
      fn $name() -> Result<(), TestFailure> {
        /// Apply one configured association that is expected to fail validation.
        fn rejected_association<'operation>(
          workspace: &'operation mut WorkspaceState<$transport<TestEnvironment>>,
          environment: &'operation TestEnvironment,
          pattern: &'static str,
          schema: &'static str,
          revision: Revision,
          context: &'static str,
        ) -> LocalFuture<'operation, Result<WorldError, TestFailure>> {
          Box::pin(async move {
            set_lsp_association(&mut workspace.config, pattern, schema);
            ensure_some(
              workspace
                .$apply_configuration(environment, Config::default(), revision)
                .await
                .err(),
              context,
            )
          })
        }

        block_on(async {
          let environment = TestEnvironment::default();
          let transport = $transport::new(
            environment.clone(),
            ensure_ok(local_http_client(), $client_context)?,
          );
          let root = WorkspaceRoot::Rooted(url("file:///workspace/")?);
          let document = url("file:///workspace/document.toml")?;
          let mut workspace = ensure_ok(
            WorkspaceState::new(root, transport.clone()),
            "the rooted configured-association workspace must construct",
          )?;
          workspace.config.schema.catalogs.clear();
          set_lsp_association(
            &mut workspace.config,
            r".*/document\.toml$",
            "./schema.json",
          );
          drop(workspace.documents.insert(
            document.clone(),
            ensure_ok(
              DocumentState::parse("value = 1\n"),
              "the configured-association document must parse",
            )?,
          ));

          let committed_revision = Revision(1);
          let notifications = ensure_ok(
            workspace
              .$apply_configuration(&environment, Config::default(), committed_revision)
              .await,
            "a rooted relative LSP association must commit",
          )?;
          let selected = ensure_some(
            workspace.schemas.associations().association_for(&document),
            "the rooted LSP association must select its configured document",
          )?;
          ensure(
            (
              selected.url.clone(),
              selected.priority,
            ) == (
              url("file:///workspace/schema.json")?,
              priority::LSP_CONFIG,
            ),
            "a rooted relative association must resolve against its workspace and retain LSP priority",
          )?;
          let selected_source = ensure_some(
            selected.meta.get("source"),
            "the selected association must retain its owning source",
          )?;
          ensure_eq(
            selected_source,
            &json!(source::LSP_CONFIG),
            "the selected association must identify the LSP configuration owner",
          )?;
          let selected_notification = ensure_some(
            notifications
              .iter()
              .find(|notification| notification.document_uri == document),
            "configuration commit must notify the open document",
          )?;
          ensure(
            selected_notification.schema_uri.as_ref() == Some(&selected.url),
            "the association notification must expose the newly selected schema",
          )?;

          let pattern_failure = rejected_association(
            &mut workspace,
            &environment,
            "[",
            "https://example.com/rejected-pattern.json",
            Revision(2),
            "an invalid configured regular expression must be rejected",
          )
          .await?;
          ensure(
            matches!(
              pattern_failure,
              WorldError::AssociationPattern {
                ref pattern,
                ..
              } if pattern == "["
            ),
            "configured regular-expression failure must retain the rejected pattern",
          )?;
          ensure_committed_association(
            &workspace,
            &document,
            &selected.url,
            committed_revision,
            "pattern validation failure must preserve the committed association and both revisions",
          )?;

          let url_failure = rejected_association(
            &mut workspace,
            &environment,
            r".*/document\.toml$",
            "::",
            Revision(3),
            "an invalid configured schema URL must be rejected",
          )
          .await?;
          ensure(
            matches!(
              url_failure,
              WorldError::AssociationUrl {
                ref source_value,
                ..
              } if source_value == "::"
            ),
            "configured URL failure must retain the rejected source value",
          )?;
          ensure_committed_association(
            &workspace,
            &document,
            &selected.url,
            committed_revision,
            "URL validation failure must preserve the committed association and both revisions",
          )?;

          workspace.config.schema.enabled = false;
          set_lsp_association(
            &mut workspace.config,
            r".*/document\.toml$",
            "./disabled.json",
          );
          let disabled_revision = Revision(4);
          let disabled_notifications = ensure_ok(
            workspace
              .$apply_configuration(&environment, Config::default(), disabled_revision)
              .await,
            "disabling schemas must atomically remove LSP-owned associations",
          )?;
          ensure(
            (
              workspace.schemas.associations().association_for(&document).is_none(),
              workspace.config_revision,
              workspace.schema_revision,
            ) == (true, disabled_revision, disabled_revision),
            "disabled schema configuration must remove prior selections and commit both revisions",
          )?;
          let disabled_notification = ensure_some(
            disabled_notifications
              .iter()
              .find(|notification| notification.document_uri == document),
            "schema disablement must notify the open document",
          )?;
          ensure(
            disabled_notification.schema_uri.is_none(),
            "schema disablement must explicitly notify the client that no schema remains selected",
          )?;

          workspace.config.schema.enabled = true;
          set_lsp_association(
            &mut workspace.config,
            r".*/document\.toml$",
            "./recovered.json",
          );
          let recovered_revision = Revision(5);
          let recovered_schema = url("file:///workspace/recovered.json")?;
          drop(ensure_ok(
            workspace
              .$apply_configuration(&environment, Config::default(), recovered_revision)
              .await,
            "configured associations must recover after schema support is re-enabled",
          )?);
          let recovered_selection = workspace
            .schemas
            .associations()
            .association_for(&document)
            .map(|association| association.url);
          ensure(
            (
              recovered_selection,
              workspace.config_revision,
              workspace.schema_revision,
            ) == (Some(recovered_schema), recovered_revision, recovered_revision),
            "re-enabled schema configuration must select the replacement association and advance both revisions",
          )?;

          let mut detached = ensure_ok(
            WorkspaceState::new(WorkspaceRoot::Detached, transport),
            "the detached configured-association workspace must construct",
          )?;
          detached.config.schema.catalogs.clear();
          set_lsp_association(&mut detached.config, r".*\.toml$", "./schema.json");
          let detached_failure = ensure_some(
            detached
              .$apply_configuration(&environment, Config::default(), Revision(1))
              .await
              .err(),
            "a detached relative LSP association must be rejected",
          )?;
          ensure(
            matches!(
              detached_failure,
              WorldError::DetachedRelativePath {
                ref path
              } if path == Path::new("./schema.json")
            ),
            "detached association failure must retain the unresolved relative path",
          )?;
          ensure(
            (
              detached.config_revision,
              detached.schema_revision,
              detached.schemas.associations().association_for(&document).is_none(),
            ) == (Revision::INITIAL, Revision::INITIAL, true),
            "detached-path rejection must preserve initial revisions and install no association",
          )
        })
      }
    };
  }

  workspace_association_contract!(
    local_workspace_lsp_associations_validate_commit_disable_and_recover,
    LocalSchemaTransport,
    "the local association client must construct",
    apply_configuration_local
  );

  #[cfg(not(target_arch = "wasm32"))]
  workspace_association_contract!(
    concurrent_workspace_lsp_associations_validate_commit_disable_and_recover,
    ConcurrentSchemaTransport,
    "the concurrent association client must construct",
    apply_configuration_concurrent
  );

  /// Load one configuration through both execution families for parity assertions.
  fn load_config_pair<'fixture>(
    environment: &'fixture TestEnvironment,
    root: &'fixture WorkspaceRoot,
    lsp_config: &'fixture LspConfig,
    default: &'fixture Config,
  ) -> LocalFuture<'fixture, Result<(Config, Config), TestFailure>> {
    Box::pin(async move {
      let local = ensure_ok(
        load_config_local(environment, root, lsp_config, default).await,
        "the local configuration loader must succeed",
      )?;
      let concurrent = ensure_ok(
        load_config_concurrent(environment, root, lsp_config, default).await,
        "the concurrent configuration loader must succeed",
      )?;
      Ok((local, concurrent))
    })
  }

  /// Load one explicit client configuration path through both execution families.
  fn load_explicit_config_pair<'fixture>(
    environment: &'fixture TestEnvironment,
    root: &'fixture WorkspaceRoot,
    default: &'fixture Config,
    configured_path: &str,
  ) -> LocalFuture<'fixture, Result<(Config, Config), TestFailure>> {
    let path = PathBuf::from(configured_path);
    Box::pin(async move {
      let mut lsp_config = ensure_ok(LspConfig::new(), "the explicit-path LSP configuration must construct")?;
      lsp_config.taplo.config_file.path = Some(path);
      load_config_pair(environment, root, &lsp_config, default).await
    })
  }

  /// Require both execution families to agree on one prepared file inclusion.
  fn ensure_configuration_inclusion(
    local: &Config,
    concurrent: &Config,
    path: &Path,
    expected: bool,
    context: &'static str,
  ) -> Result<(), TestFailure> {
    ensure(
      (local.is_included(path), concurrent.is_included(path)) == (expected, expected),
      context,
    )
  }

  #[test]
  fn configuration_loaders_share_rooted_discovery_and_explicit_path_semantics() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      environment.insert_file("/workspace/taplo.toml", b"include = [\"discovered.toml\"]\n".to_vec());
      environment.insert_file("/workspace/config/project.toml", b"include = [\"relative.toml\"]\n".to_vec());
      environment.insert_file("/configs/absolute.toml", b"include = [\"absolute.toml\"]\n".to_vec());
      let root = WorkspaceRoot::Rooted(url("file:///workspace")?);
      let default = default_config("default.toml");
      let discovered_lsp = ensure_ok(LspConfig::new(), "the discovery LSP configuration must construct")?;
      let (local_discovered, concurrent_discovered) = load_config_pair(&environment, &root, &discovered_lsp, &default).await?;
      ensure_configuration_inclusion(
        &local_discovered,
        &concurrent_discovered,
        Path::new("/workspace/discovered.toml"),
        true,
        "both loaders must prepare the discovered configuration against the rooted base",
      )?;
      ensure_configuration_inclusion(
        &local_discovered,
        &concurrent_discovered,
        Path::new("/workspace/default.toml"),
        false,
        "a discovered configuration must replace rather than merge the host default file rule",
      )?;
      ensure(
        environment.discovery_bases() == [PathBuf::from("/workspace"), PathBuf::from("/workspace")],
        "local and concurrent discovery must consult the same normalized root",
      )?;

      let (local_relative, concurrent_relative) = load_explicit_config_pair(&environment, &root, &default, "config/project.toml").await?;
      ensure_configuration_inclusion(
        &local_relative,
        &concurrent_relative,
        Path::new("/workspace/relative.toml"),
        true,
        "both loaders must resolve rooted relative paths against the workspace",
      )?;

      let (local_absolute, concurrent_absolute) =
        load_explicit_config_pair(&environment, &root, &default, "/configs/absolute.toml").await?;
      ensure_configuration_inclusion(
        &local_absolute,
        &concurrent_absolute,
        Path::new("/workspace/absolute.toml"),
        true,
        "both loaders must prepare absolute configuration contents against the workspace base",
      )?;
      ensure(
        environment.discovery_bases() == [PathBuf::from("/workspace"), PathBuf::from("/workspace")],
        "explicit configuration paths must not perform incidental discovery",
      )
    })
  }

  #[test]
  fn configuration_loaders_share_disabled_and_detached_default_behavior() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      environment.set_read_failure(true);
      let default = default_config("default.toml");
      let detached = WorkspaceRoot::Detached;
      let mut disabled = ensure_ok(LspConfig::new(), "the disabled LSP configuration must construct")?;
      disabled.taplo.config_file.enabled = false;
      disabled.taplo.config_file.path = Some(PathBuf::from("/missing/config.toml"));
      let local_disabled = ensure_ok(
        load_config_local(&environment, &detached, &disabled, &default).await,
        "disabled local loading must use the prepared host default without reading",
      )?;
      let concurrent_disabled = ensure_ok(
        load_config_concurrent(&environment, &detached, &disabled, &default).await,
        "disabled concurrent loading must use the prepared host default without reading",
      )?;
      ensure(
        (
          local_disabled.is_included(Path::new("/workspace/default.toml")),
          concurrent_disabled.is_included(Path::new("/workspace/default.toml")),
        ) == (true, true),
        "disabled loading must preserve the host default under both execution models",
      )?;

      let detached_default = ensure_ok(LspConfig::new(), "the detached-default LSP configuration must construct")?;
      let local_default = ensure_ok(
        load_config_local(&environment, &detached, &detached_default, &default).await,
        "detached local loading without a path must use the host default",
      )?;
      let concurrent_default = ensure_ok(
        load_config_concurrent(&environment, &detached, &detached_default, &default).await,
        "detached concurrent loading without a path must use the host default",
      )?;
      ensure(
        (
          local_default.is_included(Path::new("/workspace/default.toml")),
          concurrent_default.is_included(Path::new("/workspace/default.toml")),
        ) == (true, true),
        "detached default loading must prepare against the deterministic current directory",
      )?;
      ensure(
        environment.discovery_bases().is_empty(),
        "disabled and detached-default loading must not perform rooted discovery",
      )
    })
  }

  #[test]
  fn configuration_loaders_reject_invalid_host_context_and_recover_after_read_failure() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let detached = WorkspaceRoot::Detached;
      let default = default_config("default.toml");
      let mut relative = ensure_ok(LspConfig::new(), "the detached-relative LSP configuration must construct")?;
      relative.taplo.config_file.path = Some(PathBuf::from("relative.toml"));
      ensure(
        [
          matches!(
            load_config_local(&environment, &detached, &relative, &default).await,
            Err(WorldError::DetachedRelativePath { .. })
          ),
          matches!(
            load_config_concurrent(&environment, &detached, &relative, &default).await,
            Err(WorldError::DetachedRelativePath { .. })
          ),
        ] == [true, true],
        "both loaders must reject relative configuration paths for a detached workspace",
      )?;

      let missing_cwd = TestEnvironment::default();
      missing_cwd.set_cwd(None);
      let implicit = ensure_ok(LspConfig::new(), "the implicit detached LSP configuration must construct")?;
      ensure(
        [
          matches!(
            load_config_local(&missing_cwd, &detached, &implicit, &default).await,
            Err(WorldError::MissingCurrentDirectory)
          ),
          matches!(
            load_config_concurrent(&missing_cwd, &detached, &implicit, &default).await,
            Err(WorldError::MissingCurrentDirectory)
          ),
        ] == [true, true],
        "both loaders must reject detached configuration without a current directory",
      )?;

      let root = WorkspaceRoot::Rooted(url("file:///workspace")?);
      let mut explicit = ensure_ok(LspConfig::new(), "the explicit recovery LSP configuration must construct")?;
      explicit.taplo.config_file.path = Some(PathBuf::from("/configs/recovery.toml"));
      environment.insert_file("/configs/recovery.toml", b"include = [\"recovered.toml\"]\n".to_vec());
      environment.set_read_failure(true);
      ensure(
        [
          matches!(
            load_config_local(&environment, &root, &explicit, &default).await,
            Err(WorldError::Environment(EnvironmentError::Io { .. }))
          ),
          matches!(
            load_config_concurrent(&environment, &root, &explicit, &default).await,
            Err(WorldError::Environment(EnvironmentError::Io { .. }))
          ),
        ] == [true, true],
        "both loaders must preserve injected read failures as typed environment errors",
      )?;
      environment.set_read_failure(false);
      let local_recovered = ensure_ok(
        load_config_local(&environment, &root, &explicit, &default).await,
        "the local loader must recover after reads become available",
      )?;
      let concurrent_recovered = ensure_ok(
        load_config_concurrent(&environment, &root, &explicit, &default).await,
        "the concurrent loader must recover after reads become available",
      )?;
      ensure(
        (
          local_recovered.is_included(Path::new("/workspace/recovered.toml")),
          concurrent_recovered.is_included(Path::new("/workspace/recovered.toml")),
        ) == (true, true),
        "both loaders must commit the expected configuration after recovery",
      )
    })
  }

  #[test]
  fn configuration_loaders_preserve_absence_utf8_and_toml_failure_boundaries() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let root = WorkspaceRoot::Rooted(url("file:///workspace")?);
      let default = default_config("default.toml");
      let implicit = ensure_ok(LspConfig::new(), "the absent-discovery LSP configuration must construct")?;
      let (local_default, concurrent_default) = load_config_pair(&environment, &root, &implicit, &default).await?;
      ensure(
        (
          local_default.is_included(Path::new("/workspace/default.toml")),
          concurrent_default.is_included(Path::new("/workspace/default.toml")),
        ) == (true, true),
        "missing discovered configuration must retain the prepared host default",
      )?;
      ensure(
        environment.discovery_bases() == [PathBuf::from("/workspace"), PathBuf::from("/workspace")],
        "both absent discovery paths must record the same rooted base",
      )?;

      environment.insert_file("/configs/non-utf8.toml", vec![0xff]);
      let mut invalid_utf8 = ensure_ok(LspConfig::new(), "the invalid-UTF-8 LSP configuration must construct")?;
      invalid_utf8.taplo.config_file.path = Some(PathBuf::from("/configs/non-utf8.toml"));
      ensure(
        [
          matches!(
            load_config_local(&environment, &root, &invalid_utf8, &default).await,
            Err(WorldError::ConfigUtf8 { path, .. }) if path == PathBuf::from("/configs/non-utf8.toml")
          ),
          matches!(
            load_config_concurrent(&environment, &root, &invalid_utf8, &default).await,
            Err(WorldError::ConfigUtf8 { path, .. }) if path == PathBuf::from("/configs/non-utf8.toml")
          ),
        ] == [true, true],
        "both loaders must preserve the selected path in typed UTF-8 failures",
      )?;

      environment.insert_file("/configs/invalid.toml", b"include = [\n".to_vec());
      let mut invalid_toml = ensure_ok(LspConfig::new(), "the invalid-TOML LSP configuration must construct")?;
      invalid_toml.taplo.config_file.path = Some(PathBuf::from("/configs/invalid.toml"));
      ensure(
        [
          matches!(
            load_config_local(&environment, &root, &invalid_toml, &default).await,
            Err(WorldError::ConfigToml { path, .. }) if path == PathBuf::from("/configs/invalid.toml")
          ),
          matches!(
            load_config_concurrent(&environment, &root, &invalid_toml, &default).await,
            Err(WorldError::ConfigToml { path, .. }) if path == PathBuf::from("/configs/invalid.toml")
          ),
        ] == [true, true],
        "both loaders must preserve the selected path in typed TOML failures",
      )
    })
  }

  #[test]
  fn concurrent_document_snapshots_are_send_and_sync() -> Result<(), TestFailure> {
    #[cfg(not(target_arch = "wasm32"))]
    {
      require_send_sync::<DocumentSnapshot<ConcurrentSchemaTransport<TestEnvironment>>>();
      require_send_sync::<WorldState<TestEnvironment, ConcurrentSchemaTransport<TestEnvironment>>>();
    };
    ensure(true, "the target-specific concurrent type assertions must compile")
  }

  #[test]
  fn workspace_ownership_respects_url_identity_and_path_boundaries() -> Result<(), TestFailure> {
    let root = url("file:///workspace")?;
    let nested = url("file:///workspace/nested")?;
    let document = url("file:///workspace/nested/file.toml")?;
    ensure(
      (WorkspaceRoot::Detached.url(), WorkspaceRoot::Rooted(root.clone()).url()) == (None, Some(&root)),
      "workspace-root identity must distinguish detached and rooted domains",
    )?;
    ensure(
      root_contains_document(&root, &document),
      "a document below a root path boundary must be owned by that root",
    )?;
    ensure(
      root_contains_document(&root, &root),
      "a document URL exactly equal to a root path must remain owned by that root",
    )?;
    ensure(
      root_contains_document(&url("file:///")?, &document),
      "a filesystem URL root must own every absolute child path",
    )?;
    ensure(
      !root_contains_document(&root, &url("file:///workspace-other/file.toml")?),
      "a textual path prefix without a segment boundary must not establish ownership",
    )?;
    ensure(
      !root_contains_document(&root, &url("https://example.com/workspace/file.toml")?),
      "a different URL origin must not establish workspace ownership",
    )?;
    ensure(
      [
        root_contains_document(
          &url("https://user@example.com/workspace")?,
          &url("https://example.com/workspace/file.toml")?,
        ),
        root_contains_document(
          &url("https://example.com:8443/workspace")?,
          &url("https://example.com/workspace/file.toml")?,
        ),
      ] == [false, false],
      "different credentials or effective ports must not establish workspace ownership",
    )?;
    ensure(
      !root_contains_document(&url("mailto:user@example.com")?, &url("mailto:other@example.com")?),
      "non-hierarchical URLs must never establish workspace ownership",
    )?;
    ensure(
      deepest_root([&root, &nested].into_iter(), &document) == Some(&nested),
      "the deepest matching root must own the document",
    )
  }

  #[test]
  fn duplicate_roots_and_revision_exhaustion_are_typed_failures() -> Result<(), TestFailure> {
    let root = url("file:///workspace")?;
    ensure(
      matches!(
        ensure_unique_roots(&[root.clone(), root]),
        Err(WorldError::DuplicateWorkspaceRoot { .. })
      ),
      "duplicate initialization roots must be rejected before mutation",
    )?;

    let world = local_world()?;
    world.revision.store(u64::MAX, Ordering::SeqCst);
    ensure(
      matches!(world.candidate_revision(), Err(WorldError::RevisionExhausted)),
      "the world revision counter must fail rather than wrap",
    )?;

    let document = url("file:///workspace/duplicate.toml")?;
    let state = ensure_ok(DocumentState::parse("value = 1\n"), "the duplicate-ownership document must parse")?;
    let mut documents = HashMap::default();
    ensure_ok(
      insert_unique_document(&mut documents, document.clone(), state.clone()),
      "the first document owner must install",
    )?;
    let mut incoming = HashMap::default();
    incoming.extend([(document.clone(), state)]);
    ensure(
      matches!(
        merge_unique_documents(&mut documents, incoming),
        Err(WorldError::DuplicateDocumentOwnership {
          document: duplicate
        }) if duplicate == document
      ),
      "merging a second workspace seed for the same document must reject ambiguous ownership",
    )
  }

  #[test]
  fn initialization_and_scoped_configuration_validate_before_atomic_commit() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let root = url("file:///workspace/project")?;
      let preparation_environment = world.env.clone();
      drop(ensure_ok(
        WorldTransaction::new(&world, move |preparation| {
          prepare_schema_disabled(preparation, preparation_environment.clone())
        })
        .initialize_roots_with(Arc::new(InitConfig::default()), vec![root.clone()])
        .await,
        "one valid rooted topology must initialize transactionally",
      )?);
      ensure(
        world.rooted_workspace_urls().await == [root.clone()],
        "initialization must publish the complete rooted topology",
      )?;
      let debug = format!("{world:?}");
      ensure(
        (
          debug.contains("revision: 1"),
          debug.contains("rooted_workspaces: 1"),
          debug.contains("open_documents: 0"),
        ) == (true, true, true),
        "world debug output must report only stable committed topology state",
      )?;
      let topology_guard = world.workspaces.write().await;
      let locked_debug = format!("{world:?}");
      drop(topology_guard);
      ensure(
        (
          locked_debug.contains("revision: 1"),
          locked_debug.contains("rooted_workspaces"),
          locked_debug.contains("open_documents"),
        ) == (true, false, false),
        "world debug output must remain nonblocking and omit topology fields while their lock is unavailable",
      )?;

      ensure(
        matches!(
          world
            .initialize_roots_local(Arc::new(InitConfig::default()), vec![root.clone()])
            .await,
          Err(WorldError::AlreadyInitialized)
        ),
        "a second initialization must fail before replacing the committed topology",
      )?;
      let initialized_revision = world.revision.load(Ordering::SeqCst);
      let empty = ensure_ok(
        world.apply_configuration_values_local(None, &[]).await,
        "an empty configuration response must be a successful no-op",
      )?;
      ensure(
        (empty.is_empty(), world.revision.load(Ordering::SeqCst)) == (true, initialized_revision),
        "an empty configuration response must not emit effects or advance the revision",
      )?;

      let invalid_global = Value::String(String::from("invalid"));
      ensure(
        matches!(
          world
            .apply_configuration_values_local(Some(&invalid_global), &[])
            .await,
          Err(WorldError::ConfigurationResponse { ref scope, .. })
            if scope == "global workspace configuration"
        ),
        "a non-object global configuration must retain its typed scope",
      )?;
      let invalid_scoped = [(root.clone(), Value::Bool(false))];
      ensure(
        matches!(
          world
            .apply_configuration_values_local(None, &invalid_scoped)
            .await,
          Err(WorldError::ConfigurationResponse { ref scope, .. })
            if scope == root.as_str()
        ),
        "a non-object scoped configuration must retain its root scope",
      )?;
      let missing_root = url("file:///workspace/missing")?;
      let missing_scoped = [(missing_root.clone(), json!({}))];
      ensure(
        matches!(
          world
            .apply_configuration_values_local(None, &missing_scoped)
            .await,
          Err(WorldError::MissingWorkspace { root: missing }) if missing == missing_root
        ),
        "a scoped configuration for an unknown root must preserve its typed ownership failure",
      )?;
      ensure(
        world.revision.load(Ordering::SeqCst) == initialized_revision,
        "rejected configuration responses must leave the committed revision unchanged",
      )?;

      let global = json!({
        "schema": {
          "enabled": false,
          "catalogs": [],
          "links": true
        }
      });
      let scoped = [(
        root.clone(),
        json!({
          "syntax": {
            "semanticTokens": false
          }
        }),
      )];
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&global), &scoped).await,
        "valid global and scoped configuration must merge and commit together",
      )?);
      let topology = world.workspaces.read().await;
      let rooted = ensure_some(topology.rooted(&root), "the configured root must remain present after replacement")?;
      let detached = Arc::clone(&topology.detached);
      drop(topology);
      let rooted_config = rooted.read().await.config.clone();
      let detached_config = detached.read().await.config.clone();
      ensure(
        (
          rooted_config.schema.links,
          rooted_config.syntax.semantic_tokens,
          detached_config.schema.links,
          detached_config.syntax.semantic_tokens,
        ) == (true, false, true, true),
        "global configuration must reach every workspace while the scoped overlay changes only its root",
      )
    })
  }

  #[test]
  fn configuration_reclassifies_included_and_excluded_open_documents() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      world.set_default_config(Arc::new(default_config("included.toml")));
      let schema_disabled = schema_disabled_configuration(true);
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&schema_disabled), &[]).await,
        "the initial inclusion configuration must commit without schema catalogs",
      )?);
      let included = url("file:///workspace/included.toml")?;
      let excluded = url("file:///workspace/excluded.toml")?;
      let included_update = ensure_ok(
        world.replace_document(&included, "value = 1\n").await,
        "the selected document must install",
      )?;
      let excluded_update = ensure_ok(
        world.replace_document(&excluded, "value = 2\n").await,
        "the unselected document must remain tracked",
      )?;
      ensure(
        (included_update.disposition, excluded_update.disposition)
          == (super::DocumentDisposition::Included, super::DocumentDisposition::Excluded),
        "document replacement must report inclusion from the prepared file rule",
      )?;
      ensure(
        (
          world.document_snapshot(&included).await.is_some(),
          world.document_snapshot(&excluded).await.is_some(),
        ) == (true, false),
        "only included documents may expose handler snapshots",
      )?;

      world.set_default_config(Arc::new(default_config("excluded.toml")));
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&schema_disabled), &[]).await,
        "the replacement inclusion configuration must commit",
      )?);
      ensure(
        (
          world.document_snapshot(&included).await.is_some(),
          world.document_snapshot(&excluded).await.is_some(),
        ) == (false, true),
        "configuration replacement must atomically reclassify every retained open document",
      )?;
      ensure(
        world.open_document_dispositions().await
          == [
            (excluded, super::DocumentDisposition::Included),
            (included, super::DocumentDisposition::Excluded),
          ],
        "open-document dispositions must remain complete and stably ordered after reclassification",
      )
    })
  }

  #[test]
  fn document_replacement_and_close_transactions_are_idempotent_at_their_boundaries() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let document = url("file:///workspace/document.toml")?;
      let unknown = url("file:///workspace/unknown.toml")?;
      let initial_revision = world.revision.load(Ordering::SeqCst);
      let unknown_close = ensure_ok(
        world.close_document(&unknown).await,
        "closing an unknown document must be a successful no-op",
      )?;
      ensure(
        (unknown_close.is_empty(), world.revision.load(Ordering::SeqCst)) == (true, initial_revision),
        "an unknown close must emit no effects and must not advance the world revision",
      )?;

      let first = ensure_ok(
        world.replace_document(&document, "value = 1\n").await,
        "the vacant document entry must install",
      )?;
      ensure(
        first.disposition == super::DocumentDisposition::Included,
        "a default-config document must install as included",
      )?;
      let first_revision = world.revision.load(Ordering::SeqCst);
      let second = ensure_ok(
        world.replace_document(&document, "value = 2\n").await,
        "the occupied document entry must replace in place",
      )?;
      ensure(
        (second.disposition, world.revision.load(Ordering::SeqCst) > first_revision) == (super::DocumentDisposition::Included, true),
        "occupied replacement must retain inclusion and commit a later revision",
      )?;
      let snapshot = ensure_some(
        world.document_snapshot(&document).await,
        "the replaced document must retain one current snapshot",
      )?;
      ensure_eq(
        &ensure_ok(serde_json::to_value(&snapshot.document.dom), "the replacement DOM must serialize")?,
        &json!({ "value": 2 }),
        "occupied replacement must expose only the new semantic document",
      )?;

      drop(ensure_ok(
        world.close_document(&document).await,
        "closing a present document must commit",
      )?);
      let closed_revision = world.revision.load(Ordering::SeqCst);
      ensure(
        world.document_snapshot(&document).await.is_none(),
        "a committed close must remove the document snapshot",
      )?;
      let repeated = ensure_ok(
        world.close_document(&document).await,
        "closing an already closed document must remain a successful no-op",
      )?;
      ensure(
        (repeated.is_empty(), world.revision.load(Ordering::SeqCst)) == (true, closed_revision),
        "a repeated close must emit no effects and must not advance the committed revision",
      )
    })
  }

  /// A schema-independent topology fixture with one future-rooted and one detached document.
  struct TopologyFixture {
    /// World whose topology is under test.
    world:             LocalTestWorld,
    /// Valid workspace root used by the transaction.
    root:              Url,
    /// Document beneath `root`.
    rooted_document:   Url,
    /// Document outside `root`.
    detached_document: Url,
  }

  /// Build the shared topology fixture without contacting schema catalogs.
  fn topology_fixture() -> LocalFuture<'static, Result<TopologyFixture, TestFailure>> {
    Box::pin(async {
      let world = local_world()?;
      let topology = world.workspaces.read().await;
      let mut detached = topology.detached.write().await;
      detached.config.schema.enabled = false;
      detached.config.schema.catalogs.clear();
      drop(detached);
      drop(topology);

      let rooted_document = url("file:///workspace/project/document.toml")?;
      let detached_document = url("file:///outside/document.toml")?;
      drop(ensure_ok(
        world.replace_document(&rooted_document, "owner = \"root\"\n").await,
        "the future rooted document must install while detached",
      )?);
      drop(ensure_ok(
        world.replace_document(&detached_document, "owner = \"detached\"\n").await,
        "the permanently detached document must install",
      )?);
      Ok(TopologyFixture {
        world,
        root: url("file:///workspace/project")?,
        rooted_document,
        detached_document,
      })
    })
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

  #[test]
  fn topology_transactions_redistribute_documents_and_preserve_no_ops() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = topology_fixture().await?;
      let add_environment = fixture.world.env.clone();
      let added = ensure_ok(
        WorldTransaction::new(&fixture.world, move |preparation| {
          prepare_schema_disabled(preparation, add_environment.clone())
        })
        .change_roots_with(&[], from_ref(&fixture.root))
        .await,
        "adding a valid root must atomically redistribute matching documents",
      )?;
      ensure(
        (fixture.world.rooted_workspace_urls().await, added.documents)
          == (vec![fixture.root.clone()], vec![
            fixture.detached_document.clone(),
            fixture.rooted_document.clone(),
          ]),
        "the committed topology must expose the root and report every reconsidered document in stable order",
      )?;
      ensure(
        (
          fixture.world.document_snapshot(&fixture.rooted_document).await.is_some(),
          fixture.world.document_snapshot(&fixture.detached_document).await.is_some(),
        ) == (true, true),
        "redistribution must preserve both rooted and detached document snapshots",
      )?;

      let committed_revision = fixture.world.revision.load(Ordering::SeqCst);
      let repeat_environment = fixture.world.env.clone();
      let repeated_add = ensure_ok(
        WorldTransaction::new(&fixture.world, move |preparation| {
          prepare_schema_disabled(preparation, repeat_environment.clone())
        })
        .change_roots_with(&[], from_ref(&fixture.root))
        .await,
        "adding an existing root must be a successful no-op",
      )?;
      let unknown = url("file:///workspace/unknown")?;
      let unknown_remove = ensure_ok(
        fixture.world.change_roots_local(from_ref(&unknown), &[]).await,
        "removing an unknown root must be a successful no-op",
      )?;
      ensure(
        (
          repeated_add.documents.is_empty(),
          repeated_add.notifications.is_empty(),
          unknown_remove.documents.is_empty(),
          unknown_remove.notifications.is_empty(),
          fixture.world.revision.load(Ordering::SeqCst),
        ) == (true, true, true, true, committed_revision),
        "repeated add and unknown removal must emit no effects and preserve the revision",
      )?;

      let remove_environment = fixture.world.env.clone();
      let removed = ensure_ok(
        WorldTransaction::new(&fixture.world, move |preparation| {
          prepare_schema_disabled(preparation, remove_environment.clone())
        })
        .change_roots_with(from_ref(&fixture.root), &[])
        .await,
        "removing a present root must redistribute its documents to detached state",
      )?;
      ensure(
        (fixture.world.rooted_workspace_urls().await, removed.documents)
          == (Vec::<Url>::new(), vec![fixture.detached_document, fixture.rooted_document]),
        "root removal must commit an empty rooted topology and report the redistributed documents",
      )
    })
  }

  #[test]
  fn added_roots_inherit_global_configuration_while_retained_roots_preserve_scoped_overrides() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let global = schema_disabled_configuration(false);
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&global), &[]).await,
        "global configuration must commit before workspace roots are added",
      )?);

      let first_root = url("file:///workspace/first")?;
      drop(ensure_ok(
        world.change_roots_local(&[], from_ref(&first_root)).await,
        "the first root must prepare from the committed global configuration",
      )?);
      let first_scoped = [(
        first_root.clone(),
        json!({
          "syntax": {
            "semanticTokens": true
          }
        }),
      )];
      drop(ensure_ok(
        world.apply_configuration_values_local(None, &first_scoped).await,
        "the first root's scoped configuration must commit",
      )?);

      let second_root = url("file:///workspace/second")?;
      drop(ensure_ok(
        world.change_roots_local(&[], from_ref(&second_root)).await,
        "a later root must prepare from the committed global configuration",
      )?);

      let topology = world.workspaces.read().await;
      let detached = Arc::clone(&topology.detached);
      let first = ensure_some(
        topology.rooted(&first_root),
        "the retained first root must remain in the committed topology",
      )?;
      let second = ensure_some(
        topology.rooted(&second_root),
        "the added second root must enter the committed topology",
      )?;
      drop(topology);
      let detached_config = detached.read().await.config.clone();
      let first_config = first.read().await.config.clone();
      let second_config = second.read().await.config.clone();

      ensure(
        (
          detached_config.schema.enabled,
          detached_config.syntax.semantic_tokens,
          first_config.schema.enabled,
          first_config.syntax.semantic_tokens,
          second_config.schema.enabled,
          second_config.syntax.semantic_tokens,
        ) == (false, false, false, true, false, false),
        "new roots must inherit global configuration while retained roots keep their scoped overlay",
      )
    })
  }

  #[test]
  fn topology_transactions_are_failure_atomic_and_recover() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = topology_fixture().await?;
      let before_failure = fixture.world.revision.load(Ordering::SeqCst);
      let invalid_root = url("https://example.com/workspace")?;
      let failure = ensure_some(
        fixture.world.change_roots_local(&[], from_ref(&invalid_root)).await.err(),
        "a root outside the host file-path model must reject topology preparation",
      )?;
      ensure(
        matches!(failure, WorldError::InvalidWorkspaceRoot { .. }),
        "invalid root preparation must retain the typed workspace-root failure",
      )?;
      ensure(
        (
          fixture.world.rooted_workspace_urls().await,
          fixture.world.revision.load(Ordering::SeqCst),
          fixture.world.document_snapshot(&fixture.rooted_document).await.is_some(),
          fixture.world.document_snapshot(&fixture.detached_document).await.is_some(),
        ) == (Vec::<Url>::new(), before_failure, true, true),
        "failed preparation must leave roots, revision, and every document unchanged",
      )?;

      let recovery_environment = fixture.world.env.clone();
      drop(ensure_ok(
        WorldTransaction::new(&fixture.world, move |preparation| {
          prepare_schema_disabled(preparation, recovery_environment.clone())
        })
        .change_roots_with(&[], from_ref(&fixture.root))
        .await,
        "topology mutation must recover after a rejected root",
      )?);
      ensure(
        fixture.world.rooted_workspace_urls().await == [fixture.root],
        "recovery must commit the next valid rooted topology",
      )
    })
  }

  #[test]
  fn embedding_cache_default_yields_to_explicit_client_configuration() -> Result<(), TestFailure> {
    let world = local_world()?;
    let host_cache = PathBuf::from("/host/cache");
    world.set_default_cache_path(Some(host_cache.clone()));

    let inherited = world.effective_init_config(Arc::new(InitConfig {
      cache_path:            None,
      configuration_section: "client-section".into(),
    }));
    let inherited_cache_path = ensure_some(
      inherited.cache_path.as_ref(),
      "omitting cachePath must retain a configured host cache directory",
    )?;
    ensure(
      inherited_cache_path == &host_cache,
      "omitting cachePath must retain the embedding host's configured cache directory",
    )?;
    ensure_eq(
      &inherited.configuration_section,
      &String::from("client-section"),
      "host cache defaults must not replace client configuration-section selection",
    )?;

    let client_cache = PathBuf::from("/client/cache");
    let explicit = world.effective_init_config(Arc::new(InitConfig {
      cache_path:            Some(client_cache.clone()),
      configuration_section: "explicit-section".into(),
    }));
    let explicit_cache_path = ensure_some(explicit.cache_path.as_ref(), "an explicit client cachePath must remain present")?;
    ensure(
      explicit_cache_path == &client_cache,
      "an explicit client cachePath must override the embedding host default",
    )?;
    ensure_eq(
      &explicit.configuration_section,
      &String::from("explicit-section"),
      "explicit client initialization must remain otherwise unchanged",
    )
  }

  #[test]
  fn cache_configuration_propagates_clock_failures_atomically_and_recovers() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let client = ensure_ok(local_http_client(), "the cache-policy schema client must construct")?;
      let transport = LocalSchemaTransport::new(environment.clone(), client);
      let mut workspace = ensure_ok(
        WorkspaceState::new(WorkspaceRoot::Detached, transport),
        "the cache-policy workspace must construct while the host clock is available",
      )?;
      workspace.config.schema.enabled = false;
      workspace.config.schema.catalogs.clear();

      let committed = Revision(1);
      drop(ensure_ok(
        workspace
          .apply_configuration_local(&environment, Config::default(), committed)
          .await,
        "cache policy and workspace revisions must commit while the host clock is available",
      )?);
      ensure(
        (workspace.config_revision, workspace.schema_revision) == (committed, committed),
        "successful cache configuration must commit both configuration and schema revisions",
      )?;

      environment.set_clock_available(false);
      let failed_revision = Revision(2);
      let failure = ensure_some(
        workspace
          .apply_configuration_local(&environment, Config::default(), failed_revision)
          .await
          .err(),
        "an unavailable host clock must reject cache-policy application",
      )?;
      ensure(
        matches!(
          failure,
          WorldError::Cache(CacheError::Transport(TransportError::Environment(
            EnvironmentError::MissingCallback {
              name: "now"
            }
          )))
        ),
        "clock failures must retain the complete world, cache, transport, and environment error chain",
      )?;
      ensure(
        (workspace.config_revision, workspace.schema_revision) == (committed, committed),
        "failed cache configuration must not advance either committed workspace revision",
      )?;

      environment.set_clock_available(true);
      let recovered = Revision(3);
      drop(ensure_ok(
        workspace
          .apply_configuration_local(&environment, Config::default(), recovered)
          .await,
        "cache policy application must recover after the host clock returns",
      )?);
      ensure(
        (workspace.config_revision, workspace.schema_revision) == (recovered, recovered),
        "recovered cache configuration must commit both revisions at the requested generation",
      )
    })
  }

  #[test]
  fn manual_association_families_compile_select_replace_and_reject_atomically() -> Result<(), TestFailure> {
    block_on(async {
      let (world, document) = open_document_fixture("the manual-association document must install").await?;

      let glob_schema = url("https://example.com/glob.json")?;
      let glob_update = ensure_ok(
        world
          .associate_schema(
            ManualAssociationRule::Glob(String::from("**/*.toml")),
            manual_association(glob_schema.clone(), priority::LSP_CONFIG),
          )
          .await,
        "a valid global glob association must commit",
      )?;
      ensure(
        (glob_update.diagnostic_document, glob_update.notifications.is_empty()) == (None, false),
        "a global association must refresh effective associations without singling out one document",
      )?;
      let selected_glob = ensure_some(
        world.associated_schema(&document).await,
        "the matching glob association must become effective",
      )?;
      ensure(
        selected_glob.url == glob_schema,
        "the matching glob must select its configured schema",
      )?;

      let regex_schema = url("https://example.com/regex.json")?;
      drop(ensure_ok(
        world
          .associate_schema(
            ManualAssociationRule::Regex(String::from(r".*/document\.toml$")),
            manual_association(regex_schema.clone(), priority::DIRECTIVE),
          )
          .await,
        "a valid global regular-expression association must commit",
      )?);
      let selected_regex = ensure_some(
        world.associated_schema(&document).await,
        "the higher-priority regular expression must become effective",
      )?;
      ensure(
        selected_regex.url == regex_schema,
        "association priority must select the matching regular-expression schema",
      )?;
      let listed = world.list_schema_associations(&document).await;
      ensure(
        (
          listed.iter().any(|association| association.url == glob_schema),
          listed.iter().any(|association| association.url == regex_schema),
        ) == (true, true),
        "schema listing must retain both non-document manual associations",
      )?;

      let revision_before_rejections = world.revision.load(Ordering::SeqCst);
      ensure(
        matches!(
          world
            .associate_schema(
              ManualAssociationRule::Regex(String::from("[")),
              manual_association(url("https://example.com/invalid-regex.json")?, priority::MAX),
            )
            .await,
          Err(WorldError::ManualAssociationPattern { ref pattern, .. }) if pattern == "["
        ),
        "an invalid regular expression must preserve its manual-association error context",
      )?;
      ensure(
        world
          .associate_schema(
            ManualAssociationRule::Glob(String::from("[")),
            manual_association(url("https://example.com/invalid-glob.json")?, priority::MAX),
          )
          .await
          .is_err(),
        "an invalid glob must fail before mutating association state",
      )?;
      ensure(
        world.revision.load(Ordering::SeqCst) == revision_before_rejections,
        "failed manual-association compilation must leave the world revision unchanged",
      )?;

      let exact_schema = url("https://example.com/exact.json")?;
      let exact_update = ensure_ok(
        world
          .associate_schema(
            ManualAssociationRule::Url(document.clone()),
            manual_association(exact_schema.clone(), priority::MAX),
          )
          .await,
        "an exact-document association must commit",
      )?;
      ensure(
        exact_update.diagnostic_document == Some(document.clone()),
        "an exact association must identify the one document requiring diagnostics",
      )?;
      let replacement_schema = url("https://example.com/replacement.json")?;
      drop(ensure_ok(
        world
          .associate_schema(
            ManualAssociationRule::Url(document.clone()),
            manual_association(replacement_schema.clone(), priority::MAX),
          )
          .await,
        "a second exact-document association must replace its prior manual owner",
      )?);
      let selected_exact = ensure_some(
        world.associated_schema(&document).await,
        "the replacement exact association must remain effective",
      )?;
      ensure(
        selected_exact.url == replacement_schema,
        "exact-document replacement must not leave the previous schema selected",
      )?;
      ensure(
        {
          let listed_urls = world
            .list_schema_associations(&document)
            .await
            .into_iter()
            .map(|association| association.url)
            .collect::<Vec<_>>();
          (listed_urls.contains(&exact_schema), listed_urls.contains(&replacement_schema)) == (false, false)
        },
        "non-document schema listing must exclude exact document associations",
      )
    })
  }

  #[test]
  fn document_schema_and_configuration_mutations_invalidate_old_snapshots() -> Result<(), TestFailure> {
    block_on(async {
      let (world, document) = open_document_fixture("the initial document must install").await?;
      let original = ensure_some(
        world.document_snapshot(&document).await,
        "the initial document must expose a snapshot",
      )?;
      ensure(
        world.snapshot_is_current(&document, &original).await,
        "a newly captured document snapshot must be current",
      )?;

      drop(ensure_ok(
        world.replace_document(&document, "value = 2\n").await,
        "the replacement document must install",
      )?);
      ensure(
        !world.snapshot_is_current(&document, &original).await,
        "a document replacement must invalidate output captured from the old source",
      )?;
      let after_document = ensure_some(
        world.document_snapshot(&document).await,
        "the replacement document must expose a snapshot",
      )?;

      let schema_url = url("https://example.com/schema.json")?;
      drop(ensure_ok(
        world
          .associate_schema(ManualAssociationRule::Url(document.clone()), SchemaAssociation {
            url:      schema_url,
            meta:     json!({ "source": source::MANUAL }),
            priority: priority::MAX,
          })
          .await,
        "the manual schema association must commit",
      )?);
      ensure(
        !world.snapshot_is_current(&document, &after_document).await,
        "a schema-association mutation must invalidate old generation-dependent output",
      )?;
      let after_schema = ensure_some(
        world.document_snapshot(&document).await,
        "the associated document must retain a snapshot",
      )?;

      let configuration = json!({
        "schema": { "enabled": false },
        "syntax": { "semanticTokens": false }
      });
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&configuration), &[]).await,
        "the offline client configuration must commit",
      )?);
      ensure(
        !world.snapshot_is_current(&document, &after_schema).await,
        "a configuration mutation must invalidate old generation-dependent output",
      )?;
      let current = ensure_some(
        world.document_snapshot(&document).await,
        "the reconfigured document must retain a current snapshot",
      )?;
      ensure(
        world.snapshot_is_current(&document, &current).await,
        "a snapshot captured after every committed mutation must be current",
      )?;

      drop(ensure_ok(
        world.close_document(&document).await,
        "closing the document must commit",
      )?);
      ensure(
        !world.snapshot_is_current(&document, &current).await,
        "closing a document must suppress every previously captured result",
      )
    })
  }
}
