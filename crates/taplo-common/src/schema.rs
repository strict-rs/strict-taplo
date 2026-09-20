use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::error::Error as StdError;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::mem::take;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use futures::future::LocalBoxFuture;
use itertools::Itertools as _;
use json_value_merge::Merge as _;
use jsonschema::Retrieve;
use jsonschema::Uri;
use jsonschema::ValidationError;
use jsonschema::Validator;
use jsonschema::error::ValidationErrorKind;
use jsonschema::paths::LocationSegment;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;
use taplo::dom;
use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::dom::node::Key;
use taplo::rowan::TextRange;
use thiserror::Error;
use tracing::Instrument as _;
use url::Url;

use self::associations::AssociationError;
use self::associations::SchemaAssociations;
use self::builtins::builtin_schema;
use self::cache::Cache;
use self::cache::CacheError;
#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
use self::transport::ConcurrentSchemaTransport;
use self::transport::ConcurrentTransport;
#[cfg(feature = "reqwest")]
use self::transport::LocalSchemaTransport;
use self::transport::OfflineSchemaTransport;
use self::transport::SchemaTransport;
use self::transport::TransportError;
use crate::LruCache;
#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
use crate::environment::ConcurrentEnvironment;
use crate::environment::LocalEnvironment;
use crate::util::ArcHashValue;

/// Generate paired local and concurrent schema operation families from aligned operation names.
macro_rules! schema_execution_families {
  (
    $operations:ident;
    $(($local:tt, $concurrent:tt)),+ $(,)?
  ) => {
    $operations!($($local),+; LocalBoxFuture, boxed_local, {});
    $operations!(
      $($concurrent),+;
      BoxFuture,
      boxed,
      {
        where
          T: ConcurrentTransport,
          for<'transport> T::ReadBytesFuture<'transport>: Send,
          for<'transport> T::ReadFuture<'transport>: Send,
          for<'transport> T::WriteFuture<'transport>: Send
      }
    );
  };
}

pub mod associations;
pub mod cache;
pub mod ext;
pub mod transport;

/// Shared process-local schema documents used by caches and validators.
type SharedSchemaStore = Arc<Mutex<LruCache<Url, Arc<Value>>>>;

/// Shared compiled-validator cache.
type ValidatorStore = Arc<Mutex<LruCache<Url, Arc<Validator>>>>;

/// One schema branch resolved at an absolute document path and reference base.
type ResolvedSchema = (Keys, Arc<Value>, Url);

/// One descendant schema with absolute path, relative path, value, and reference base.
type ResolvedChildSchema = (Keys, Keys, Arc<Value>, Url);

/// One completion-facing descendant schema: the absolute document path, the path relative to the
/// queried path, and the schema.
type PossibleSchema = (Keys, Keys, Arc<Value>);

/// A typed failure while interpreting, resolving, or validating JSON Schema.
#[derive(Debug, Error)]
pub enum SchemaError {
  /// Cache access failed.
  #[error(transparent)]
  Cache(#[from] CacheError),
  /// Transport access failed.
  #[error(transparent)]
  Transport(#[from] TransportError),
  /// Association construction or refresh failed.
  #[error(transparent)]
  Association(#[from] AssociationError),
  /// A DOM value could not be converted into JSON for validation.
  #[error("TOML DOM could not be converted into a JSON validation value")]
  DomSerialization {
    /// Underlying serializer failure.
    #[source]
    source: serde_json::Error,
  },
  /// A built-in schema could not be serialized.
  #[error("built-in schema `{url}` could not be serialized")]
  BuiltinSerialization {
    /// Built-in schema URL.
    url:    String,
    /// Underlying serializer failure.
    #[source]
    source: serde_json::Error,
  },
  /// A schema document could not be compiled.
  #[error("invalid schema `{url}`: {message}")]
  InvalidSchema {
    /// Schema URL.
    url:     Url,
    /// Stable validator diagnostic.
    message: String,
  },
  /// A schema reference could not be converted into an absolute URL.
  #[error("schema reference `{reference}` cannot be resolved against `{root}`")]
  InvalidReference {
    /// Root schema URL.
    root:      Url,
    /// Rejected reference.
    reference: String,
  },
  /// A resolved schema fragment does not exist.
  #[error("schema fragment `{fragment}` does not exist in `{url}`")]
  MissingFragment {
    /// Document URL without the fragment.
    url:      Url,
    /// Missing JSON pointer fragment.
    fragment: String,
  },
  /// A schema path contains an invalid regular expression.
  #[error("invalid `patternProperties` expression `{pattern}` at `{path}` in `{url}`")]
  InvalidPattern {
    /// Root schema URL.
    url:     Box<Url>,
    /// Current document path.
    path:    Box<Keys>,
    /// Rejected expression.
    pattern: String,
    /// Underlying regex failure.
    #[source]
    source:  Box<regex::Error>,
  },
  /// A validator instance path could not be projected onto the TOML DOM.
  #[error("validation path `{path}` does not exist in the TOML DOM")]
  InvalidNodePath {
    /// Invalid dotted/indexed path.
    path: Keys,
  },
  /// A remote load failed and no usable stale cache entry existed.
  #[error("schema `{url}` is unavailable: {transport}")]
  Unavailable {
    /// Requested schema URL.
    url:       Url,
    /// Primary transport failure.
    transport: Box<TransportError>,
    /// Stale-cache failure, when one was available.
    cache:     Option<Box<CacheError>>,
  },
}

/// Schema, reference base, and cycle state shared by every schema traversal.
#[derive(Debug)]
struct SchemaTraversalState {
  /// Current schema branch.
  schema:   Arc<Value>,
  /// Base URL used to resolve identifiers and references.
  base_url: Url,
  /// References already visited on this branch.
  visited:  HashSet<Url>,
}

/// Reference classification for one schema traversal frame.
enum TraversalReference {
  /// The current schema is not a reference and retains this derived base URL.
  NotReference {
    /// Base URL for ordinary keywords on the current schema.
    base_url: Url,
  },
  /// The reference target has not appeared on this traversal branch.
  Unvisited {
    /// Fully resolved target URL inserted into the branch's cycle set.
    url: Url,
  },
  /// The reference target already appeared on this traversal branch.
  Visited,
}

impl SchemaTraversalState {
  /// Construct one traversal root.
  fn root(schema: Arc<Value>, base_url: Url) -> Self {
    Self {
      schema,
      base_url,
      visited: HashSet::new(),
    }
  }

  /// Replace the current schema and base while preserving owned cycle state.
  fn successor(self, schema: Arc<Value>, base_url: Url) -> Self {
    Self {
      schema,
      base_url,
      visited: self.visited,
    }
  }

  /// Clone cycle state for one independently explored schema branch.
  fn branch(&self, schema: Arc<Value>, base_url: Url) -> Self {
    Self {
      schema,
      base_url,
      visited: self.visited.clone(),
    }
  }

  /// Replace traversal state after composition has resolved additional references.
  #[allow(
    clippy::single_call_fn,
    reason = "the merged constructor is the only successor that adopts cycle state an intersection branch resolved outside the frame it \
              continues"
  )]
  const fn merged(schema: Arc<Value>, base_url: Url, visited: HashSet<Url>) -> Self {
    Self {
      schema,
      base_url,
      visited,
    }
  }

  /// Classify the current schema's optional reference and advance cycle state.
  fn classify_reference(&mut self) -> Result<TraversalReference, SchemaError> {
    let base_url = schema_base_url(&self.base_url, &self.schema)?;
    let Some(reference) = self.schema.schema_ref() else {
      return Ok(TraversalReference::NotReference {
        base_url,
      });
    };
    let url = reference_url(&base_url, reference)?;
    if self.visited.insert(url.clone()) {
      Ok(TraversalReference::Unvisited {
        url,
      })
    } else {
      Ok(TraversalReference::Visited)
    }
  }
}

/// Resolve or stop one traversal task at its current optional schema reference.
macro_rules! follow_schema_reference {
  ($schemas:expr, $resolve_schema:ident, $task:ident, $work:ident => $base_url:ident) => {
    let $base_url = match $task.traversal.classify_reference()? {
      TraversalReference::NotReference {
        base_url,
      } => base_url,
      TraversalReference::Unvisited {
        url,
      } => {
        let schema = $schemas.$resolve_schema(url.clone()).await?;
        $work.push($task.reference_successor(schema, &url));
        return Ok(());
      }
      TraversalReference::Visited => return Ok(()),
    };
  };
}

/// One heap-owned schema/path traversal frame.
#[derive(Debug)]
struct SchemaPathTask {
  /// Shared schema branch, reference base, and cycle state.
  traversal:      SchemaTraversalState,
  /// Current JSON instance value.
  instance:       Value,
  /// Absolute TOML path already traversed.
  full_path:      Keys,
  /// TOML path still to traverse.
  remaining_path: Keys,
}

impl SchemaPathTask {
  /// Construct the root frame for one path query.
  fn root(schema: Arc<Value>, base_url: Url, instance: Value, remaining_path: Keys) -> Self {
    Self {
      traversal: SchemaTraversalState::root(schema, base_url),
      instance,
      full_path: Keys::empty(),
      remaining_path,
    }
  }

  /// Follow one previously unvisited schema reference.
  fn reference_successor(self, schema: Arc<Value>, resolved_url: &Url) -> Self {
    Self {
      traversal:      self.traversal.successor(schema, document_base_url(resolved_url)),
      instance:       self.instance,
      full_path:      self.full_path,
      remaining_path: self.remaining_path,
    }
  }

  /// Explore one schema-composition branch without advancing the document path.
  fn composition_successor(&self, schema: Arc<Value>, base_url: Url) -> Self {
    Self {
      traversal:      self.traversal.branch(schema, base_url),
      instance:       self.instance.clone(),
      full_path:      self.full_path.clone(),
      remaining_path: self.remaining_path.clone(),
    }
  }

  /// Advance through one named property candidate.
  fn property_successor(&self, schema: Arc<Value>, base_url: Url, instance: Value, full_path: Keys, remaining_path: Keys) -> Self {
    Self {
      traversal: self.traversal.branch(schema, base_url),
      instance,
      full_path,
      remaining_path,
    }
  }

  /// Advance through one array index candidate.
  fn index_successor(self, schema: Arc<Value>, base_url: Url, instance: Value, index: usize, remaining_path: Keys) -> Self {
    Self {
      traversal: self.traversal.successor(schema, base_url),
      instance,
      full_path: self.full_path.join(index),
      remaining_path,
    }
  }
}

/// One heap-owned descendant-schema traversal frame.
#[derive(Debug)]
struct ChildSchemaTask {
  /// Absolute TOML path at which schema exploration began.
  root_path: Keys,
  /// Shared schema branch, reference base, and cycle state.
  traversal: SchemaTraversalState,
  /// Relative path from `root_path`.
  path:      Keys,
  /// Remaining descendant depth.
  depth:     usize,
}

impl ChildSchemaTask {
  /// Construct a descendant traversal root.
  fn root(root_path: Keys, schema: Arc<Value>, base_url: Url, depth: usize) -> Self {
    Self {
      root_path,
      traversal: SchemaTraversalState::root(schema, base_url),
      path: Keys::empty(),
      depth,
    }
  }

  /// Follow one previously unvisited descendant reference.
  fn reference_successor(self, schema: Arc<Value>, resolved_url: &Url) -> Self {
    Self {
      root_path: self.root_path,
      traversal: self.traversal.successor(schema, document_base_url(resolved_url)),
      path:      self.path,
      depth:     self.depth,
    }
  }

  /// Explore one composition branch at the current descendant path.
  fn composition_successor(&self, schema: Arc<Value>, base_url: Url) -> Self {
    Self {
      root_path: self.root_path.clone(),
      traversal: self.traversal.branch(schema, base_url),
      path:      self.path.clone(),
      depth:     self.depth,
    }
  }

  /// Continue through a merged `allOf` schema at the current path.
  fn merged_successor(self, schema: Arc<Value>, base_url: Url, visited: HashSet<Url>) -> Self {
    Self {
      root_path: self.root_path,
      traversal: SchemaTraversalState::merged(schema, base_url, visited),
      path:      self.path,
      depth:     self.depth,
    }
  }

  /// Advance through one named descendant property.
  fn property_successor(&self, key: &str, schema: Arc<Value>, base_url: Url, depth: usize) -> Self {
    Self {
      root_path: self.root_path.clone(),
      traversal: self.traversal.branch(schema, base_url),
      path: self.path.join(Key::from(key)),
      depth,
    }
  }
}

/// A JSON Schema composition keyword with nonempty branches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositionKind {
  /// Intersection branches.
  Intersection,
  /// Exclusive-alternative branches.
  ExclusiveAlternative,
  /// Alternative branches.
  Alternative,
}

impl CompositionKind {
  /// Return this keyword's branch array, or an empty slice when absent or malformed.
  fn branches(self, schema: &Value) -> &[Value] {
    let name = match self {
      Self::Intersection => "allOf",
      Self::ExclusiveAlternative => "oneOf",
      Self::Alternative => "anyOf",
    };
    schema.get(name).and_then(Value::as_array).map_or(&[], Vec::as_slice)
  }
}

/// Recognize a metadata wrapper around exactly one nonempty composition keyword.
fn composition_only_kind(schema: &Value) -> Option<CompositionKind> {
  let object = schema.as_object()?;
  let has_own_properties = object
    .get("properties")
    .and_then(Value::as_object)
    .is_some_and(|properties| !properties.is_empty());
  if has_own_properties {
    return None;
  }

  let mut kinds = [
    CompositionKind::Intersection,
    CompositionKind::ExclusiveAlternative,
    CompositionKind::Alternative,
  ]
  .into_iter()
  .filter(|kind| !kind.branches(schema).is_empty());
  let kind = kinds.next()?;
  kinds.next().is_none().then_some(kind)
}

/// Collect `patternProperties` branches matching one semantic key.
#[allow(
  clippy::single_call_fn,
  reason = "pattern-property collection isolates fallible regex compilation from path-frame successor construction"
)]
fn matching_pattern_schemas(schema_url: &Url, child_path: &Keys, key: &Key, schema: &Value) -> Result<Vec<Value>, SchemaError> {
  let Some(pattern_properties) = schema.get("patternProperties").and_then(Value::as_object) else {
    return Ok(Vec::new());
  };
  let mut candidates = Vec::new();
  for (pattern, pattern_schema) in pattern_properties {
    let regex = Regex::new(pattern).map_err(|source| SchemaError::InvalidPattern {
      url:     Box::new(schema_url.clone()),
      path:    Box::new(child_path.clone()),
      pattern: pattern.clone(),
      source:  Box::new(source),
    })?;
    if regex.is_match(key.value()) {
      candidates.push(pattern_schema.clone());
    }
  }
  Ok(candidates)
}

/// Clone one direct schema member or use JSON `null` when it is absent.
fn schema_member(schema: &Value, name: &str) -> Value {
  schema.get(name).cloned().unwrap_or(Value::Null)
}

/// Clone one named member from an object-valued schema container.
fn nested_schema_member(schema: &Value, container: &str, name: &str) -> Value {
  schema
    .get(container)
    .and_then(|members| members.get(name))
    .cloned()
    .unwrap_or(Value::Null)
}

/// Schemas Taplo serves from the binary itself, without transport or cache.
pub mod builtins {
  use std::sync::Arc;

  use serde_json::Value;
  use url::Url;

  use super::SchemaError;
  use crate::config::Config;

  /// URL of Taplo's generated configuration schema.
  pub const TAPLO_CONFIG_URL: &str = "taplo://taplo.toml";

  /// Build Taplo's configuration schema.
  ///
  /// # Errors
  ///
  /// Returns [`SchemaError`] when the generated schema cannot be serialized.
  #[allow(
    clippy::single_call_fn,
    reason = "the public configuration-schema factory exposes the generated built-in independently of URL resolution"
  )]
  pub fn taplo_config_schema() -> Result<Arc<Value>, SchemaError> {
    serde_json::to_value(schemars::schema_for!(Config))
      .map(Arc::new)
      .map_err(|source| SchemaError::BuiltinSerialization {
        url: TAPLO_CONFIG_URL.into(),
        source,
      })
  }

  /// Return a built-in schema for one URL.
  ///
  /// # Errors
  ///
  /// Returns [`SchemaError`] if the generated built-in cannot be serialized.
  #[allow(
    clippy::single_call_fn,
    reason = "the built-in resolver is the URL-classification boundary shared with cache-backed schema loading"
  )]
  pub fn builtin_schema(url: &Url) -> Result<Option<Arc<Value>>, SchemaError> {
    if url.as_str() == TAPLO_CONFIG_URL {
      taplo_config_schema().map(Some)
    } else {
      Ok(None)
    }
  }
}

/// Generate one execution-model-specific schema operation family.
macro_rules! schema_operations {
  (
    $validate_root:ident,
    $validate:ident,
    $get_or_build_validator:ident,
    $load_schema:ident,
    $add_validator:ident,
    $resolve_schema:ident,
    $create_validator:ident,
    $schemas_at_path:ident,
    $schemas_at_path_with_bases:ident,
    $advance_schema_path_task:ident,
    $advance_child_schema_task:ident,
    $possible_schemas_from:ident,
    $cache_load:ident,
    $cache_store:ident;
    $future:ident,
    $box_with:ident,
    { $($bounds:tt)* }
  ) => {
    /// Validate a parsed TOML document and report each error against its DOM node.
    ///
    /// The returned errors carry the offending node and its source ranges, which is what a
    /// diagnostic-producing caller needs; [`Schemas::validate`] is the JSON-level equivalent.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] when the DOM cannot be converted into JSON, the schema cannot be
    /// loaded or compiled, or a reported instance path does not exist in the DOM.
    pub fn $validate_root<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      root: &'schemas dom::Node,
    ) -> $future<'schemas, Result<Vec<NodeValidationError>, SchemaError>>
    $($bounds)*
    {
      let span = tracing::info_span!(stringify!($validate_root), %schema_url);
      async move {
        let instance = serde_json::to_value(root).map_err(|source| SchemaError::DomSerialization {
          source,
        })?;
        self
          .$validate(schema_url, &instance)
          .await?
          .into_iter()
          .map(|error| NodeValidationError::new(root, error))
          .collect::<Result<Vec<_>, _>>()
      }
      .instrument(span)
      .$box_with()
    }

    /// Validate a JSON value against one schema and return every error it reports.
    ///
    /// An empty result means the value is valid. The compiled validator is cached, so repeated
    /// validation against the same schema URL does no further loading.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] when the schema cannot be loaded, one of its references is
    /// unreachable, or the document does not compile into a validator.
    pub fn $validate<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      instance: &'schemas Value,
    ) -> $future<'schemas, Result<Vec<SchemaValidationError>, SchemaError>>
    $($bounds)*
    {
      let span = tracing::info_span!(stringify!($validate), %schema_url);
      async move {
        // External `$ref`s are resolved eagerly when the validator is built
        // (see `create_validator`), so validation itself is a pure, synchronous pass.
        let validator = self.$get_or_build_validator(schema_url).await?;
        Ok(
          validator
            .iter_errors(instance)
            .map(|error| SchemaValidationError::from_jsonschema(&error))
            .collect(),
        )
      }
      .instrument(span)
      .$box_with()
    }

    /// Return a cached validator or build and install it from the owning schema.
    fn $get_or_build_validator<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
    ) -> $future<'schemas, Result<Arc<Validator>, SchemaError>>
    $($bounds)*
    {
      async move {
        if let Some(validator) = self.get_validator(schema_url)? {
          return Ok(validator);
        }

        let schema = self.$load_schema(schema_url).await?;
        self.add_schema(schema_url, Arc::clone(&schema));
        self.$add_validator(schema_url.clone(), &schema).await
      }
      .$box_with()
    }

    /// Load one schema document from the cache, the built-ins, or the transport.
    ///
    /// A live cache entry is returned without contacting the transport. Otherwise the document
    /// is fetched and cached; if the fetch fails, an expired cache entry is preferred over
    /// failing, so a previously seen schema keeps working offline.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::Unavailable`] when the transport fails and no cache entry —
    /// expired or not — can stand in, or another [`SchemaError`] when caching fails.
    pub fn $load_schema<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
    ) -> $future<'schemas, Result<Arc<Value>, SchemaError>>
    $($bounds)*
    {
      let span = tracing::info_span!(stringify!($load_schema), %schema_url);
      async move {
        if let Ok(cached_schema) = self.cache.$cache_load(schema_url, false).await {
          tracing::debug!(%schema_url, "schema was found in cache");
          return Ok(cached_schema);
        }

        let schema = if let Some(builtin) = builtin_schema(schema_url)? {
          builtin
        } else {
          match self.transport.read_json(schema_url.clone()).await {
            Ok(loaded_schema) => Arc::new(loaded_schema),
            Err(transport) => match self.cache.$cache_load(schema_url, true).await {
              Ok(stale_schema) => {
                tracing::debug!(%schema_url, "expired schema was found in cache");
                return Ok(stale_schema);
              }
              Err(cache) => {
                return Err(SchemaError::Unavailable {
                  url: schema_url.clone(),
                  transport: Box::new(transport),
                  cache: Some(Box::new(cache)),
                });
              }
            },
          }
        };

        self
          .cache
          .$cache_store(schema_url.clone(), Arc::clone(&schema))
          .await?;
        Ok(schema)
      }
      .instrument(span)
      .$box_with()
    }

    /// Compile and install one validator for an already loaded schema.
    fn $add_validator<'schemas>(
      &'schemas self,
      schema_url: Url,
      schema: &'schemas Value,
    ) -> $future<'schemas, Result<Arc<Validator>, SchemaError>>
    $($bounds)*
    {
      async move {
        let validator = Arc::new(self.$create_validator(&schema_url, schema).await?);
        drop(self.validators.lock().put(schema_url, Arc::clone(&validator)));
        Ok(validator)
      }
      .$box_with()
    }

    /// Resolve a schema document or JSON-pointer fragment without recursion.
    pub(crate) fn $resolve_schema(&self, mut url: Url) -> $future<'_, Result<Arc<Value>, SchemaError>>
    $($bounds)*
    {
      async move {
        let requested_fragment = url.fragment().map(ToOwned::to_owned);
        url.set_fragment(None);
        let schema = self.$load_schema(&url).await?;
        match requested_fragment {
          Some(fragment) => {
            let pointer = format!("/{}", fragment.trim_start_matches('/'));
            schema
              .pointer(&pointer)
              .cloned()
              .map(Arc::new)
              .ok_or_else(|| SchemaError::MissingFragment {
                url,
                fragment,
              })
          }
          None => Ok(schema),
        }
      }
      .$box_with()
    }

    /// Compile a validator, resolving external `$ref`s at build time.
    ///
    /// `jsonschema` resolves references synchronously through the [`Retrieve`] trait, but our
    /// schema fetching is async (network / `Environment` I/O). `CacheRetriever` therefore serves
    /// only the in-memory cache and records any reference it could not satisfy; when the build
    /// fails on a missing reference we fetch it asynchronously, cache it, and rebuild. The
    /// `attempted` set guarantees progress (and termination) for genuinely unresolvable refs.
    fn $create_validator<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      schema: &'schemas Value,
    ) -> $future<'schemas, Result<Validator, SchemaError>>
    $($bounds)*
    {
      async move {
        let mut attempted: HashSet<Url> = HashSet::new();

        loop {
          let missing = Arc::new(Mutex::new(Vec::new()));
          let retriever = CacheRetriever {
            store:   self.cache().memory_store(),
            missing: Arc::clone(&missing),
          };

          let build_result = jsonschema::options()
            .with_retriever(retriever)
            .with_format("semver", formats::semver)
            .with_format("semver-requirement", formats::semver_req)
            .should_validate_formats(true)
            .build(schema);

          let error = match build_result {
            Ok(validator) => return Ok(validator),
            Err(error) => error,
          };
          let requested = take(&mut *missing.lock());
          let fresh: Vec<Url> = requested.into_iter().filter(|url| attempted.insert(url.clone())).collect();

          if fresh.is_empty() {
            return Err(SchemaError::InvalidSchema {
              url:     schema_url.clone(),
              message: error.to_string(),
            });
          }

          for missing_url in fresh {
            // Loading is what populates the in-memory cache the retriever reads; the document
            // itself is picked up by the next build attempt.
            drop(self.$load_schema(&missing_url).await?);
          }
        }
      }
      .$box_with()
    }

    /// Resolve every schema that applies to one path inside a document.
    ///
    /// The path is walked from the schema root, following `$ref`s and composition keywords, so
    /// several schemas can apply to the same path — one per surviving `anyOf`/`oneOf` branch,
    /// for example. `instance` is the document being edited; it selects branches whose
    /// applicability depends on the current contents. Results are deduplicated and each carries
    /// the absolute path it was resolved at.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] when a schema or reference cannot be loaded or resolved, or when
    /// a `patternProperties` expression is not a valid regular expression.
    pub fn $schemas_at_path<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      instance: &'schemas Value,
      path: &'schemas Keys,
    ) -> $future<'schemas, Result<Vec<(Keys, Arc<Value>)>, SchemaError>>
    $($bounds)*
    {
      let span = tracing::info_span!(stringify!($schemas_at_path), %schema_url, %path);
      async move {
        Ok(
          self
            .$schemas_at_path_with_bases(schema_url, instance, path)
            .await?
            .into_iter()
            .map(|(resolved_path, schema, _base_url)| (resolved_path, schema))
            .collect(),
        )
      }
      .instrument(span)
      .$box_with()
    }

    /// Resolve schemas at one path while retaining each branch's reference base.
    fn $schemas_at_path_with_bases<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      instance: &'schemas Value,
      path: &'schemas Keys,
    ) -> $future<'schemas, Result<Vec<ResolvedSchema>, SchemaError>>
    $($bounds)*
    {
      async move {
        let mut resolved_schemas = Vec::new();
        let schema = self.$load_schema(schema_url).await?;
        let mut work = vec![SchemaPathTask::root(
          schema,
          schema_url.clone(),
          instance.clone(),
          path.clone(),
        )];

        while let Some(task) = work.pop() {
          self
            .$advance_schema_path_task(schema_url, task, &mut work, &mut resolved_schemas)
            .await?;
        }

        Ok(
          resolved_schemas
            .into_iter()
            .unique_by(|resolved| {
              (
                resolved.0.clone(),
                ArcHashValue(Arc::clone(&resolved.1)),
                resolved.2.clone(),
              )
            })
            .collect(),
        )
      }
      .$box_with()
    }

    /// Advance one path-resolution frame and enqueue every live successor.
    fn $advance_schema_path_task<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      mut task: SchemaPathTask,
      work: &'schemas mut Vec<SchemaPathTask>,
      resolved_schemas: &'schemas mut Vec<ResolvedSchema>,
    ) -> $future<'schemas, Result<(), SchemaError>>
    $($bounds)*
    {
      async move {
        if !task.traversal.schema.is_object() {
          return Ok(());
        }
        follow_schema_reference!(self, $resolve_schema, task, work => base_url);

        let composition = composition_only_kind(&task.traversal.schema);
        let preserve_all_of_wrapper = task.remaining_path.is_empty() && composition == Some(CompositionKind::Intersection);
        if !preserve_all_of_wrapper {
          let branches = [
            CompositionKind::Intersection,
            CompositionKind::Alternative,
            CompositionKind::ExclusiveAlternative,
          ]
          .into_iter()
          .flat_map(|kind| kind.branches(&task.traversal.schema));
          for branch in branches {
            work.push(task.composition_successor(Arc::new(branch.clone()), base_url.clone()));
          }
        }

        let Some(segment) = task.remaining_path.iter().next().cloned() else {
          if !matches!(
            composition,
            Some(CompositionKind::ExclusiveAlternative | CompositionKind::Alternative)
          ) {
            resolved_schemas.push((task.full_path, task.traversal.schema, base_url));
          }
          return Ok(());
        };

        let child_path = task.remaining_path.skip_left(1);
        match segment {
          KeyOrIndex::Key(property_key) => {
            let child_full_path = task.full_path.join(property_key.clone());
            let child_instance = task.instance.get(property_key.value()).cloned().unwrap_or(Value::Null);
            let mut candidates = vec![
              nested_schema_member(&task.traversal.schema, "items", property_key.value()),
              nested_schema_member(&task.traversal.schema, "properties", property_key.value()),
              schema_member(&task.traversal.schema, "additionalProperties"),
            ];
            candidates.extend(matching_pattern_schemas(
              schema_url,
              &child_full_path,
              &property_key,
              &task.traversal.schema,
            )?);
            for candidate in candidates.into_iter().rev() {
              work.push(task.property_successor(
                Arc::new(candidate),
                base_url.clone(),
                child_instance.clone(),
                child_full_path.clone(),
                child_path.clone(),
              ));
            }
          }
          KeyOrIndex::Index(index) => {
            let child_instance = task.instance.get(index).cloned().unwrap_or(Value::Null);
            let items = schema_member(&task.traversal.schema, "items");
            let candidate = items.as_array().and_then(|entries| entries.get(index)).cloned().unwrap_or(items);
            work.push(task.index_successor(
              Arc::new(candidate),
              base_url,
              child_instance,
              index,
              child_path,
            ));
          }
        }
        Ok(())
      }
      .$box_with()
    }

    /// Advance one descendant-schema frame and enqueue every live successor.
    fn $advance_child_schema_task<'schemas>(
      &'schemas self,
      mut task: ChildSchemaTask,
      work: &'schemas mut Vec<ChildSchemaTask>,
      children: &'schemas mut Vec<ResolvedChildSchema>,
    ) -> $future<'schemas, Result<(), SchemaError>>
    $($bounds)*
    {
      async move {
        if !task.traversal.schema.is_object() || task.depth == 0 {
          return Ok(());
        }
        follow_schema_reference!(self, $resolve_schema, task, work => base_url);

        if let Some(composition) = composition_only_kind(&task.traversal.schema) {
          let branches = composition.branches(&task.traversal.schema);
          if composition == CompositionKind::Intersection {
            let mut merged = Value::Object(serde_json::Map::default());
            let mut visited = task.traversal.visited.clone();
            for branch in branches {
              let branch_base = schema_base_url(&base_url, branch)?;
              if let Some(reference) = branch.schema_ref() {
                let url = reference_url(&branch_base, reference)?;
                if visited.insert(url.clone()) {
                  let resolved = self.$resolve_schema(url).await?;
                  merged.merge(&resolved);
                }
              } else {
                merged.merge(branch);
              }
            }
            let mut wrapper = (*task.traversal.schema).clone();
            if let Some(object) = wrapper.as_object_mut() {
              // The branches were merged above; only the wrapper's own metadata is still needed.
              drop(object.remove("allOf"));
            }
            merged.merge(&wrapper);
            work.push(task.merged_successor(Arc::new(merged), base_url, visited));
          } else {
            for branch in branches.iter().rev() {
              work.push(task.composition_successor(Arc::new(branch.clone()), base_url.clone()));
            }
          }
          return Ok(());
        }

        for composition in [
          CompositionKind::Intersection,
          CompositionKind::Alternative,
          CompositionKind::ExclusiveAlternative,
        ] {
          for branch in composition.branches(&task.traversal.schema).iter().rev() {
            work.push(task.composition_successor(Arc::new(branch.clone()), base_url.clone()));
          }
        }

        children.push((
          task.root_path.extend(task.path.clone()),
          task.path.clone(),
          Arc::clone(&task.traversal.schema),
          base_url.clone(),
        ));

        let child_depth = task.depth.saturating_sub(1);
        if let Some(properties) = task.traversal.schema.get("properties").and_then(Value::as_object) {
          for (property_name, child_schema) in properties.iter().rev() {
            work.push(task.property_successor(
              property_name,
              Arc::new(child_schema.clone()),
              base_url.clone(),
              child_depth,
            ));
          }
        }
        Ok(())
      }
      .$box_with()
    }

    /// Resolve the schemas at one path together with their descendant schemas.
    ///
    /// This is the completion-facing query: it starts from [`Schemas::schemas_at_path`] and walks
    /// up to `max_depth` levels of `properties` below each result, so a caller can offer the keys
    /// that may still be written. Each item is `(absolute path, path relative to the queried
    /// path, schema)`; `max_depth` of zero yields nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] when a schema or reference cannot be loaded or resolved.
    pub fn $possible_schemas_from<'schemas>(
      &'schemas self,
      schema_url: &'schemas Url,
      instance: &'schemas Value,
      path: &'schemas Keys,
      max_depth: usize,
    ) -> $future<'schemas, Result<Vec<PossibleSchema>, SchemaError>>
    $($bounds)*
    {
      let span = tracing::info_span!(stringify!($possible_schemas_from), %schema_url, %path);
      async move {
        let schemas = self.$schemas_at_path_with_bases(schema_url, instance, path).await?;
        let mut children: Vec<ResolvedChildSchema> = Vec::with_capacity(schemas.len());
        let mut work: Vec<ChildSchemaTask> = schemas
          .into_iter()
          .rev()
          .map(|(root_path, schema, base_url)| ChildSchemaTask::root(root_path, schema, base_url, max_depth))
          .collect();

        while let Some(task) = work.pop() {
          self.$advance_child_schema_task(task, &mut work, &mut children).await?;
        }

        Ok(
          children
            .into_iter()
            .unique_by(|child| {
              (
                child.0.clone(),
                child.1.clone(),
                ArcHashValue(Arc::clone(&child.2)),
                child.3.clone(),
              )
            })
            .map(|(root_path, relative_path, schema, _base_url)| (root_path, relative_path, schema))
            .collect(),
        )
      }
      .instrument(span)
      .$box_with()
    }
  };
}

/// The schema services shared by every Taplo tool, parameterized by transport.
///
/// One value owns the association set that maps documents to schemas, the schema cache, and a
/// small LRU of compiled validators. Cloning is cheap and shares all of that state, so a tool
/// constructs one instance and hands out clones.
///
/// The transport decides which schema locations are reachable and which execution model
/// applies: [`Schemas::new_offline`] serves `file`, `taplo`, and cached schemas only, while
/// [`Schemas::new_local`] and [`Schemas::new_concurrent`] add HTTP for current-thread and
/// multi-thread hosts respectively.
#[derive(Clone)]
pub struct Schemas<T: SchemaTransport> {
  /// Transport used to reach schema documents.
  transport:    T,
  /// Document-to-schema association set.
  associations: SchemaAssociations<T>,
  /// Compiled validators, keyed by schema URL.
  validators:   ValidatorStore,
  /// Memory and optional disk schema cache.
  cache:        Cache<T>,
}

impl<T: SchemaTransport> Debug for Schemas<T> {
  /// Render how many validators are compiled, without their contents.
  ///
  /// State another thread is currently using is reported as `None` instead of being waited
  /// for, so formatting never blocks.
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    let cached_validators = self.validators.try_lock().map(|validators| validators.len());
    f.debug_struct("Schemas")
      .field("cached_validators", &cached_validators)
      .finish_non_exhaustive()
  }
}

impl<T: SchemaTransport> Schemas<T> {
  /// Construct schema services around an explicit transport.
  ///
  /// # Errors
  ///
  /// Returns [`SchemaError`] when the cache clock or built-in association set
  /// cannot be initialized.
  pub fn with_transport(transport: T) -> Result<Self, SchemaError> {
    let cache = Cache::new(transport.clone())?;
    let associations = SchemaAssociations::new(transport.clone(), cache.clone())?;
    Ok(Self {
      transport,
      associations,
      validators: Arc::new(Mutex::new(LruCache::with_hasher(
        NonZeroUsize::new(3).unwrap_or(NonZeroUsize::MIN),
        RandomState::new(),
      ))),
      cache,
    })
  }

  /// Get a reference to the schemas's associations.
  #[must_use]
  pub const fn associations(&self) -> &SchemaAssociations<T> {
    &self.associations
  }

  /// Get a reference to the schemas's cache.
  #[must_use]
  pub const fn cache(&self) -> &Cache<T> {
    &self.cache
  }

  /// Borrow the execution-model-specific transport.
  #[must_use]
  pub const fn transport(&self) -> &T {
    &self.transport
  }

  /// Insert an in-memory schema without requiring persistence.
  pub fn add_schema(&self, schema_url: &Url, schema: Arc<Value>) {
    self.cache.insert_memory(schema_url.clone(), schema);
  }

  /// Return one compiled validator and clear validator state with an expired schema LRU.
  fn get_validator(&self, schema_url: &Url) -> Result<Option<Arc<Validator>>, SchemaError> {
    if self.cache().lru_expired()? {
      self.validators.lock().clear();
    }

    Ok(self.validators.lock().get(schema_url).cloned())
  }

  schema_execution_families!(
    schema_operations;
    (validate_root, validate_root_concurrent),
    (validate, validate_concurrent),
    (get_or_build_validator, get_or_build_validator_concurrent),
    (load_schema, load_schema_concurrent),
    (add_validator, add_validator_concurrent),
    (resolve_schema, resolve_schema_concurrent),
    (create_validator, create_validator_concurrent),
    (schemas_at_path, schemas_at_path_concurrent),
    (schemas_at_path_with_bases, schemas_at_path_with_bases_concurrent),
    (advance_schema_path_task, advance_schema_path_task_concurrent),
    (advance_child_schema_task, advance_child_schema_task_concurrent),
    (possible_schemas_from, possible_schemas_from_concurrent),
    (load, load_concurrent),
    (store, store_concurrent),
  );
}

impl<E: LocalEnvironment> Schemas<OfflineSchemaTransport<E>> {
  /// Construct schema services without HTTP/HTTPS transport.
  ///
  /// # Errors
  ///
  /// Returns [`SchemaError`] when cache or built-in initialization fails.
  pub fn new_offline(environment: E) -> Result<Self, SchemaError> {
    Self::with_transport(OfflineSchemaTransport::new(environment))
  }
}

#[cfg(feature = "reqwest")]
impl<E: LocalEnvironment> Schemas<LocalSchemaTransport<E>> {
  /// Construct current-thread schema services with HTTP transport.
  ///
  /// # Errors
  ///
  /// Returns [`SchemaError`] when cache or built-in initialization fails.
  #[allow(
    clippy::single_call_fn,
    reason = "the current-thread constructor is the public seam that selects the local execution model's transport for an embedding host"
  )]
  pub fn new_local(environment: E, http: reqwest::Client) -> Result<Self, SchemaError> {
    Self::with_transport(LocalSchemaTransport::new(environment, http))
  }
}

#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
impl<E: ConcurrentEnvironment> Schemas<ConcurrentSchemaTransport<E>> {
  /// Construct multi-thread schema services with native HTTP transport.
  ///
  /// # Errors
  ///
  /// Returns [`SchemaError`] when cache or built-in initialization fails.
  #[allow(
    clippy::single_call_fn,
    reason = "the multi-thread constructor is the public seam that selects the native execution model's transport for an embedding host"
  )]
  pub fn new_concurrent(environment: E, http: reqwest::Client) -> Result<Self, SchemaError> {
    Self::with_transport(ConcurrentSchemaTransport::new(environment, http))
  }
}

/// Resolve an optional schema `$id` against its inherited reference base.
fn schema_base_url(inherited: &Url, schema: &Value) -> Result<Url, SchemaError> {
  schema
    .get("$id")
    .and_then(Value::as_str)
    .map_or_else(|| Ok(inherited.clone()), |identifier| reference_url(inherited, identifier))
}

/// Remove a selected fragment so nested references remain relative to their owning document.
fn document_base_url(url: &Url) -> Url {
  let mut base = url.clone();
  base.set_fragment(None);
  base
}

/// Resolve a JSON Schema reference against its owning document URL.
fn reference_url(root_url: &Url, reference: &str) -> Result<Url, SchemaError> {
  if reference.starts_with('#') {
    let mut url = root_url.clone();
    url.set_fragment(Some(reference.trim_start_matches('#')));
    return Ok(url);
  }
  Url::parse(reference)
    .or_else(|_| root_url.join(reference))
    .map_err(|_parse_or_join_error| SchemaError::InvalidReference {
      root:      root_url.clone(),
      reference: reference.into(),
    })
}

/// Convenience accessors for JSON Schema reference objects.
pub trait ValueExt {
  /// Return whether this value contains a string `$ref`.
  fn is_schema_ref(&self) -> bool;
  /// Return this value's string `$ref`, when present.
  fn schema_ref(&self) -> Option<&str>;
}

impl ValueExt for Value {
  fn is_schema_ref(&self) -> bool {
    self.get("$ref").is_some_and(Self::is_string)
  }

  fn schema_ref(&self) -> Option<&str> {
    self.get("$ref").and_then(Self::as_str)
  }
}

/// A synchronous [`Retrieve`] that serves only the in-memory schema cache.
///
/// `jsonschema` resolves `$ref`s synchronously while building a validator, but our schema
/// fetching is async. Any reference not already cached is recorded in `missing` and reported as
/// an error; [`Schemas::create_validator`] uses that list to fetch the schema and rebuild.
struct CacheRetriever {
  /// Shared schema documents available to the synchronous validator builder.
  store:   SharedSchemaStore,
  /// References the validator builder requested before they were cached.
  missing: Arc<Mutex<Vec<Url>>>,
}

/// Typed failure emitted through `jsonschema`'s boxed retrieval boundary.
#[derive(Debug, Error)]
enum CacheRetrieveError {
  /// The validator requested a malformed URL.
  #[error("validator requested invalid schema URI `{uri}`")]
  InvalidUri {
    /// Rejected URI.
    uri:    String,
    /// Underlying URL parser failure.
    #[source]
    source: url::ParseError,
  },
  /// The requested schema has not been prefetched yet.
  #[error("schema `{url}` is not cached yet")]
  Missing {
    /// Missing schema URL.
    url: Url,
  },
}

impl Retrieve for CacheRetriever {
  fn retrieve(&self, uri: &Uri<String>) -> Result<Value, Box<dyn StdError + Send + Sync>> {
    let url = Url::parse(uri.as_str()).map_err(|source| -> Box<dyn StdError + Send + Sync> {
      Box::new(CacheRetrieveError::InvalidUri {
        uri: uri.as_str().into(),
        source,
      })
    })?;
    let Some(schema) = self.store.lock().get(&url).cloned() else {
      self.missing.lock().push(url.clone());
      return Err(Box::new(CacheRetrieveError::Missing {
        url,
      }));
    };
    Ok((*schema).clone())
  }
}

/// An owned schema-validation error, decoupled from the `jsonschema` error types.
#[derive(Debug, Clone)]
pub struct SchemaValidationError {
  /// The offending property names when this is an "additional/unexpected properties" error.
  pub additional_properties: Option<Vec<String>>,
  /// The instance location that failed validation.
  pub instance_path:         Vec<PathSegment>,
  /// A human-readable description of the error.
  pub message:               String,
}

/// A single segment of a JSON instance path.
#[derive(Debug, Clone)]
pub enum PathSegment {
  /// Object property name.
  Property(String),
  /// Array element index.
  Index(usize),
}

impl SchemaValidationError {
  /// Convert a borrowed validator diagnostic into Taplo-owned path segments.
  #[allow(
    clippy::single_call_fn,
    reason = "validation conversion decouples owned diagnostics from jsonschema's borrowed error representation"
  )]
  fn from_jsonschema(error: &ValidationError<'_>) -> Self {
    let additional_properties = if let ValidationErrorKind::AdditionalProperties {
      ref unexpected,
    } = *error.kind()
    {
      Some(unexpected.clone())
    } else {
      None
    };

    let instance_path = error
      .instance_path()
      .into_iter()
      .map(|segment| match segment {
        LocationSegment::Property(property) => PathSegment::Property(property.to_string()),
        LocationSegment::Index(index) => PathSegment::Index(index),
      })
      .collect();

    Self {
      additional_properties,
      instance_path,
      message: error.to_string(),
    }
  }
}

/// A validation error resolved to a DOM node and its text ranges.
#[derive(Debug)]
pub struct NodeValidationError {
  /// Path of the offending node inside the document.
  pub keys:                  Keys,
  /// The offending node itself.
  pub node:                  dom::Node,
  /// Human-readable description of the failed constraint.
  pub message:               String,
  /// Whether the error reports unexpected properties, which changes how
  /// [`text_ranges`](NodeValidationError::text_ranges) selects source ranges.
  pub additional_properties: bool,
}

/// Project one property segment through a semantic table node.
#[allow(
  clippy::single_call_fn,
  reason = "property projection isolates table lookup and path advancement from mixed property and index traversal"
)]
fn project_property_node(node: &dom::Node, keys: &mut Keys, property: &str) -> Result<dom::Node, SchemaError> {
  let invalid_path = || SchemaError::InvalidNodePath {
    path: keys.join(Key::from(property)),
  };
  let table = node.as_table().ok_or_else(invalid_path)?;
  let (key, entry) = table
    .entries()
    .iter()
    .find(|entry| entry.0.value() == property)
    .ok_or_else(invalid_path)?;
  *keys = keys.join(key);
  Ok(entry)
}

impl NodeValidationError {
  /// Project one owned JSON validation error onto the immutable TOML DOM.
  #[allow(
    clippy::single_call_fn,
    reason = "DOM projection isolates schema-path traversal and source-node recovery from validator execution"
  )]
  fn new(root: &dom::Node, error: SchemaValidationError) -> Result<Self, SchemaError> {
    let mut keys = Keys::empty();
    let mut node = root.clone();

    if let Some(unexpected) = error.additional_properties.as_ref() {
      keys = keys.extend(unexpected.iter().map(Key::from).map(KeyOrIndex::Key));
    }

    for segment in &error.instance_path {
      match *segment {
        PathSegment::Property(ref property) => node = project_property_node(&node, &mut keys, property)?,
        PathSegment::Index(index) => {
          node = node.get_index(index).ok_or_else(|| SchemaError::InvalidNodePath {
            path: keys.join(index)
          })?;
          keys = keys.join(index);
        }
      }
    }

    Ok(Self {
      additional_properties: error.additional_properties.is_some(),
      keys,
      node,
      message: error.message,
    })
  }

  /// Return concrete source ranges associated with this validation error.
  #[must_use]
  pub fn text_ranges(&self) -> Vec<TextRange> {
    if self.additional_properties {
      let include_children = false;

      if self.keys.is_empty() {
        return self.node.text_ranges(include_children).collect();
      }

      self
        .keys
        .clone()
        .into_iter()
        .filter_map(|key| self.node.get(&key))
        .flat_map(|node| node.text_ranges(include_children))
        .collect()
    } else {
      self.node.text_ranges(true).collect()
    }
  }
}

/// JSON Schema custom format validators.
mod formats {
  #[allow(
    clippy::single_call_fn,
    reason = "the semver callback provides a named validator identity for jsonschema format registration"
  )]
  /// Return whether one string is a semantic version.
  pub(super) fn semver(candidate: &str) -> bool {
    semver::Version::parse(candidate).is_ok()
  }

  #[allow(
    clippy::single_call_fn,
    reason = "the semver-requirement callback provides a distinct validator identity for jsonschema format registration"
  )]
  /// Return whether one string is a semantic-version requirement.
  pub(super) fn semver_req(requirement: &str) -> bool {
    semver::VersionReq::parse(requirement).is_ok()
  }
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;

  use futures::executor::block_on;
  use jsonschema::Retrieve as _;
  use jsonschema::Uri;
  use parking_lot::Mutex;
  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;
  use taplo::dom::Keys;
  use taplo::dom::Node;
  use taplo::dom::QueryError;
  use taplo::parser::Parse;
  use taplo::parser::ParseFailure;
  use taplo::parser::parse;
  use thiserror::Error;
  use url::ParseError;
  use url::Url;

  use super::CacheRetrieveError;
  use super::CacheRetriever;
  use super::NodeValidationError;
  use super::PathSegment;
  use super::SchemaError;
  use super::SchemaValidationError;
  use super::Schemas;
  use super::ValueExt as _;
  use super::builtins;
  use super::transport::OfflineSchemaTransport;
  use super::transport::TransportError;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use super::transport::concurrent_http_client;
  #[cfg(feature = "reqwest")]
  use super::transport::local_http_client;
  use crate::test_support::TestEnvironment;
  /// Offline schema service used by behavior tests.
  type TestSchemas = Schemas<OfflineSchemaTransport<TestEnvironment>>;

  /// Native failures while constructing schema fixtures.
  #[derive(Debug, Error)]
  enum FixtureError {
    /// A fixture URL failed to parse.
    #[error(transparent)]
    Url(#[from] ParseError),
    /// A fixture key path failed to parse.
    #[error(transparent)]
    Keys(#[from] QueryError),
    /// A fixture syntax tree could not be built.
    #[error(transparent)]
    Parse(#[from] ParseFailure),
    /// A source fixture retained recoverable syntax diagnostics.
    #[error(transparent)]
    Syntax(#[from] Box<PredicateFailure<Parse>>),
    /// Schema initialization failed with its native context.
    #[error(transparent)]
    Schema(Box<SchemaError>),
    /// Fixture serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
  }

  impl From<SchemaError> for FixtureError {
    fn from(source: SchemaError) -> Self {
      Self::Schema(Box::new(source))
    }
  }

  impl From<TransportError> for FixtureError {
    fn from(source: TransportError) -> Self {
      Self::from(SchemaError::from(source))
    }
  }

  /// Parse a fixture URL, preserving its native parser error.
  fn url(input: &str) -> Result<Url, FixtureError> {
    Ok(Url::parse(input)?)
  }

  /// Parse a fixture key path, preserving its native query error.
  fn keys(input: &str) -> Result<Keys, FixtureError> {
    Ok(input.parse()?)
  }

  /// Construct one typed object-property and array-index path.
  fn indexed_keys(property: &str, index: usize) -> Result<Keys, FixtureError> {
    keys(property).map(|path| path.join(index))
  }

  /// Install one complete JSON schema into the native in-memory cache.
  fn seed(schemas: &TestSchemas, schema_url: &Url, schema: Value) {
    schemas.add_schema(schema_url, Arc::new(schema));
  }

  /// Construct offline schema services for one behavior fixture.
  fn offline_schemas() -> Result<TestSchemas, FixtureError> {
    Ok(Schemas::new_offline(TestEnvironment::default())?)
  }

  /// Construct offline services retaining the URL of their seeded native schema.
  fn seeded_schemas(schema_url: &str, schema: Value) -> Result<(TestSchemas, Url), FixtureError> {
    let parsed_url = url(schema_url)?;
    let schemas = offline_schemas()?;
    seed(&schemas, &parsed_url, schema);
    Ok((schemas, parsed_url))
  }

  /// Construct seeded services around one named object-property schema.
  fn seeded_object_property_schemas(
    schema_url: &str,
    property: &str,
    property_schema: &Value,
    additional_properties: &Value,
  ) -> Result<(TestSchemas, Url), FixtureError> {
    seeded_schemas(schema_url, object_property_schema(property, property_schema, additional_properties))
  }

  /// Seed the array-of-integers object schema shared by the validation-behavior tests.
  fn validation_schemas() -> Result<(TestSchemas, Url), FixtureError> {
    seeded_object_property_schemas(
      "https://example.com/validation.json",
      "values",
      &json!({
        "type": "array",
        "items": { "type": "integer" }
      }),
      &Value::Bool(false),
    )
  }

  /// Read one textual property from a schema fixture.
  fn schema_text<'a>(schema: &'a Value, name: &str) -> Option<&'a str> {
    schema.get(name).and_then(Value::as_str)
  }

  /// Project a single-property validation path into its stable property name.
  #[allow(
    clippy::single_call_fn,
    reason = "the projection names the exhaustive instance-path shape that keeps custom-format rejections attributable to one property"
  )]
  fn single_property_path(path: &[PathSegment]) -> Option<&str> {
    match *path {
      [PathSegment::Property(ref property)] => Some(property),
      [PathSegment::Index(_)] | [] | [_, _, ..] => None,
    }
  }

  /// Build one object schema with a named property and explicit fallback policy.
  #[allow(
    clippy::single_call_fn,
    reason = "the fixture builder keeps declared-property and additionalProperties shape construction separate from seeding a schema \
              service"
  )]
  fn object_property_schema(property: &str, property_schema: &Value, additional_properties: &Value) -> Value {
    json!({
      "type": "object",
      "additionalProperties": additional_properties,
      "properties": {
        (property): property_schema
      }
    })
  }

  /// Build one schema branch that contributes a single named property.
  fn property_branch(property: &str, property_schema: &Value) -> Value {
    json!({
      "properties": {
        (property): property_schema
      }
    })
  }

  /// Build a two-branch composition fixture from named property contracts.
  #[allow(
    clippy::single_call_fn,
    reason = "the builder names the two-branch composition-only wrapper shape independently of the keyword chosen and the URL it is \
              seeded under"
  )]
  fn paired_composition(kind: &str, first: &(&str, &str), second: &(&str, &str)) -> Value {
    json!({
      (kind): [
        property_branch(first.0, &json!({ "title": first.1 })),
        property_branch(second.0, &json!({ "title": second.1 }))
      ]
    })
  }

  /// Seed one two-branch composition schema and return its parsed URL.
  fn seed_composition(
    schemas: &TestSchemas,
    schema_url: &str,
    kind: &str,
    first: &(&str, &str),
    second: &(&str, &str),
  ) -> Result<Url, FixtureError> {
    let parsed_url = url(schema_url)?;
    seed(schemas, &parsed_url, paired_composition(kind, first, second));
    Ok(parsed_url)
  }

  /// Parse one syntax-clean schema-validation DOM fixture.
  fn parse_valid_dom(source: &str, context: &'static str) -> Result<Node, FixtureError> {
    Ok(
      ensure_that(parse(source)?, context, |parsed| parsed.diagnostics().is_empty())
        .map_err(Box::new)?
        .into_dom(),
    )
  }

  #[test]
  fn builtins_references_and_validator_retrieval_keep_their_public_contracts() -> Result<(), impl Debug> {
    let observed = (|| {
      let builtin_url = url(builtins::TAPLO_CONFIG_URL)?;
      let external_url = url("https://example.com/not-built-in.json")?;
      let missing_url = url("https://example.com/missing-schema.json")?;
      let schemas = offline_schemas()?;
      let generated = builtins::taplo_config_schema();
      let resolved = builtins::builtin_schema(&builtin_url);
      let external = builtins::builtin_schema(&external_url);
      let references = [
        json!({"$ref": "definitions.json#/$defs/value"}),
        json!({"$ref": 7}),
        json!({"type": "string"}),
      ];
      let debug = format!("{schemas:?}");
      let missing = Arc::new(Mutex::new(Vec::new()));
      let retriever = CacheRetriever {
        store:   schemas.cache().memory_store(),
        missing: Arc::clone(&missing),
      };
      let relative_uri = Uri::<String>::parse(String::from("http://[v1.fe80]"));
      let relative = relative_uri.as_ref().ok().map(|uri| retriever.retrieve(uri));
      let requested_uri = Uri::<String>::parse(missing_url.to_string());
      let absent = requested_uri.as_ref().ok().map(|uri| retriever.retrieve(uri));
      let requested = missing.lock().clone();
      seed(&schemas, &missing_url, json!({"title": "cached"}));
      let prefetched = requested_uri.as_ref().ok().map(|uri| retriever.retrieve(uri));
      Ok::<_, FixtureError>((
        schemas,
        (generated, resolved, external),
        references,
        debug,
        (relative_uri, relative, requested_uri, absent, requested, prefetched),
        missing_url,
      ))
    })();
    ensure_that(
      observed,
      "builtins and validator retrieval must preserve reference classification, native retrieval errors, requested URLs, and cached values",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let (ref generated, ref resolved, ref external) = actual.1;
        let [ref reference, ref non_string, ref absent_reference] = actual.2;
        let Some(Err(ref invalid_uri)) = actual.4.1 else {
          return false;
        };
        let Some(Err(ref missing_schema)) = actual.4.3 else {
          return false;
        };
        generated.as_ref().is_ok_and(|schema| {
          schema.is_object()
            && resolved
              .as_ref()
              .is_ok_and(|builtin| builtin.as_ref().is_some_and(|value| value == schema))
        }) && matches!(external, Ok(None))
          && reference.is_schema_ref()
          && reference.schema_ref() == Some("definitions.json#/$defs/value")
          && !non_string.is_schema_ref()
          && absent_reference.schema_ref().is_none()
          && actual.3.contains("cached_validators")
          && matches!(
            invalid_uri.downcast_ref::<CacheRetrieveError>(),
            Some(CacheRetrieveError::InvalidUri { .. })
          )
          && invalid_uri.to_string().contains("invalid schema URI")
          && matches!(missing_schema.downcast_ref::<CacheRetrieveError>(), Some(CacheRetrieveError::Missing { url }) if url == &actual.5)
          && missing_schema.to_string().contains("is not cached yet")
          && actual.4.4 == [actual.5.clone()]
          && actual.4.5.as_ref().is_some_and(|retrieved| {
            retrieved
              .as_ref()
              .is_ok_and(|value| schema_text(value, "title") == Some("cached"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn validation_caches_compiled_validators_and_clears_them_on_expiration() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let (schemas, schema_url) = validation_schemas()?;
      let valid = schemas.validate(&schema_url, &json!({"values": [1, 2]})).await;
      let first = schemas.get_validator(&schema_url);
      let repeated = schemas.validate(&schema_url, &json!({"values": []})).await;
      let second = schemas.get_validator(&schema_url);
      let policy = schemas.cache().set_expiration_times(Duration::ZERO, Duration::from_secs(60));
      let expired = schemas.get_validator(&schema_url);
      Ok::<_, FixtureError>((schemas, schema_url, valid, first, repeated, second, policy, expired))
    });
    ensure_that(
      observed,
      "validations must reuse the same compiled validator until schema expiration clears it",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.2.as_ref().is_ok_and(Vec::is_empty)
          && actual.4.as_ref().is_ok_and(Vec::is_empty)
          && actual
            .3
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .zip(actual.5.as_ref().ok().and_then(Option::as_ref))
            .is_some_and(|(initial, reused)| Arc::ptr_eq(initial, reused))
          && actual.6.is_ok()
          && matches!(&actual.7, Ok(None))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn validation_projects_object_and_array_failures_onto_json_and_dom() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let (schemas, schema_url) = validation_schemas()?;
      let root = parse_valid_dom(
        "values = [1, \"wrong\"]\nunexpected = true\n",
        "the DOM validation fixture must parse cleanly",
      )?;
      let array_path = indexed_keys("values", 1)?;
      let malformed = parse("values = 999999999999999999999999999999\n")?.into_dom();
      let invalid = schemas
        .validate(&schema_url, &json!({"values": [1, "wrong"], "unexpected": true}))
        .await;
      let projected = schemas.validate_root(&schema_url, &root).await;
      let serialization = schemas.validate_root(&schema_url, &malformed).await;
      Ok::<_, FixtureError>((schemas, root, array_path, malformed, invalid, projected, serialization))
    });
    ensure_that(observed, "validation must retain object and indexed failures, project both to concrete DOM ranges, and reject unserializable DOM", |result| {
      result.as_ref().is_ok_and(|actual| actual.4.as_ref().is_ok_and(|errors| {
        errors.iter().any(|error| matches!(error.instance_path.as_slice(), [PathSegment::Property(property), PathSegment::Index(1)] if property == "values"))
          && errors.iter().any(|error| error.additional_properties.as_ref().is_some_and(|properties| properties == &["unexpected"]))
      }) && actual.5.as_ref().is_ok_and(|errors| errors.iter().any(|error| error.keys == actual.2 && !error.text_ranges().is_empty())
        && errors.iter().any(|error| error.additional_properties && !error.text_ranges().is_empty()))
        && matches!(&actual.6, Err(SchemaError::DomSerialization { .. })))
    }).map(drop).map_err(Box::new)
  }

  #[test]
  fn schema_paths_distinguish_tuple_indices_and_additional_property_fallbacks() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let (schemas, schema_url) = seeded_object_property_schemas(
        "https://example.com/indexed.json",
        "values",
        &json!({"items": [{"title": "first"}, {"title": "second"}]}),
        &json!({"title": "fallback"}),
      )?;
      let paths = [indexed_keys("values", 1)?, indexed_keys("values", 4)?, keys("other")?];
      let instance = json!({"values": [true, 7], "other": "text"});
      let [ref second, ref outside, ref other] = paths;
      let results = [
        schemas.schemas_at_path(&schema_url, &instance, second).await,
        schemas.schemas_at_path(&schema_url, &instance, outside).await,
        schemas.schemas_at_path(&schema_url, &instance, other).await,
      ];
      Ok::<_, FixtureError>((schemas, instance, paths, results))
    });
    ensure_that(
      observed,
      "tuple indices must select only their own schema and undeclared properties must use the fallback",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref second, ref outside, ref other] = actual.3;
        second.as_ref().is_ok_and(|schemas| {
          schemas
            .iter()
            .any(|resolved| schema_text(&resolved.1, "title") == Some("second"))
            && !schemas
              .iter()
              .any(|resolved| schema_text(&resolved.1, "title") == Some("first"))
        }) && outside.as_ref().is_ok_and(Vec::is_empty)
          && other.as_ref().is_ok_and(|schemas| {
            schemas
              .iter()
              .any(|resolved| schema_text(&resolved.1, "title") == Some("fallback"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn composition_only_wrappers_expose_alternative_branches_and_descendants() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let schemas = offline_schemas()?;
      let one = seed_composition(
        &schemas,
        "https://example.com/composition.json",
        "oneOf",
        &("choice", "one-of text"),
        &("choice", "one-of number"),
      )?;
      let any = seed_composition(
        &schemas,
        "https://example.com/alternatives.json",
        "anyOf",
        &("left", "left alternative"),
        &("right", "right alternative"),
      )?;
      let path = keys("choice")?;
      let choices = schemas.schemas_at_path(&one, &json!({"choice": null}), &path).await;
      let descendants = schemas.possible_schemas_from(&any, &json!({}), &Keys::empty(), 2).await;
      Ok::<_, FixtureError>((schemas, choices, descendants))
    });
    ensure_that(
      observed,
      "composition-only oneOf and anyOf wrappers must preserve both independent branches and their descendants",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let Ok(ref children) = actual.2 else {
          return false;
        };
        actual.1.as_ref().is_ok_and(|choices| {
          ["one-of text", "one-of number"]
            .iter()
            .all(|title| choices.iter().any(|resolved| schema_text(&resolved.1, "title") == Some(*title)))
        }) && [("left", "left alternative"), ("right", "right alternative")]
          .iter()
          .all(|&(path, title)| {
            children
              .iter()
              .any(|child| child.1.dotted() == path && schema_text(&child.2, "title") == Some(title))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn external_references_prefetch_and_fail_with_typed_load_outcomes() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let environment = TestEnvironment::default();
      let bytes = serde_json::to_vec(&json!({"type": "string", "minLength": 2}))?;
      environment.insert_file("/workspace/referenced.json", bytes);
      let schemas = Schemas::new_offline(environment)?;
      let root_url = url("https://example.com/reference-root.json")?;
      let missing_url = url("https://example.com/missing-reference-root.json")?;
      let invalid_url = url("https://example.com/invalid-schema.json")?;
      seed(&schemas, &root_url, json!({"$ref": "file:///workspace/referenced.json"}));
      seed(&schemas, &missing_url, json!({"$ref": "https://example.com/unavailable.json"}));
      seed(&schemas, &invalid_url, json!({"type": 7}));
      let valid = schemas.validate(&root_url, &json!("valid")).await;
      let invalid = schemas.validate(&root_url, &json!("x")).await;
      let missing = schemas.validate(&missing_url, &Value::Null).await;
      let malformed = schemas.validate(&invalid_url, &Value::Null).await;
      Ok::<_, FixtureError>((schemas, valid, invalid, missing, malformed))
    });
    ensure_that(
      observed,
      "external references must prefetch, enforce constraints, and preserve unavailable-reference and invalid-schema failures",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.as_ref().is_ok_and(Vec::is_empty)
          && actual
            .2
            .as_ref()
            .is_ok_and(|errors| errors.iter().any(|error| error.message.contains("shorter than")))
          && matches!(&actual.3, Err(SchemaError::Unavailable { .. }))
          && matches!(&actual.4, Err(SchemaError::InvalidSchema { .. }))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn fragment_resolution_and_stale_cache_recovery_have_typed_outcomes() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let environment = TestEnvironment::default();
      let schemas = Schemas::new_offline(environment.clone())?;
      let root = url("https://example.com/fragments.json")?;
      let selected_url = url("https://example.com/fragments.json#/$defs/leaf")?;
      let missing_url = url("https://example.com/fragments.json#/$defs/missing")?;
      let stale_url = url("https://example.com/stale.json")?;
      seed(&schemas, &root, json!({"$defs": {"leaf": {"title": "fragment leaf"}}}));
      let selected = schemas.resolve_schema(selected_url).await;
      let missing = schemas.resolve_schema(missing_url).await;
      schemas.cache().set_cache_path(Some(PathBuf::from("/cache")));
      let policy = schemas
        .cache()
        .set_expiration_times(Duration::from_secs(1), Duration::from_secs(1));
      let saved = schemas
        .cache()
        .save(stale_url.clone(), Arc::new(json!({"title": "stale fallback"})))
        .await;
      let deadline = time::OffsetDateTime::UNIX_EPOCH.checked_add(time::Duration::seconds(1));
      if let Some(instant) = deadline {
        environment.set_now(instant);
      }
      let reader = Schemas::new_offline(environment);
      let stale = if let Ok(ref fresh) = reader {
        fresh.cache().set_cache_path(Some(PathBuf::from("/cache")));
        Some(fresh.load_schema(&stale_url).await)
      } else {
        None
      };
      Ok::<_, FixtureError>((schemas, selected, missing, policy, saved, deadline, reader, stale))
    });
    ensure_that(
      observed,
      "fragment selection and stale recovery must preserve exact native outcomes at the expiration deadline",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual
          .1
          .as_ref()
          .is_ok_and(|schema| schema_text(schema, "title") == Some("fragment leaf"))
          && matches!(&actual.2, Err(SchemaError::MissingFragment { fragment, .. }) if fragment == "/$defs/missing")
          && actual.3.is_ok()
          && actual.4.is_ok()
          && actual.5.is_some()
          && actual.7.as_ref().is_some_and(|stale| {
            stale
              .as_ref()
              .is_ok_and(|schema| schema_text(schema, "title") == Some("stale fallback"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn custom_semver_formats_accept_valid_values_and_reject_invalid_values() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let (schemas, schema_url) = seeded_schemas(
        "https://example.com/semantic-versions.json",
        json!({
          "type": "object", "required": ["version", "requirement"],
          "properties": {"version": {"type": "string", "format": "semver"}, "requirement": {"type": "string", "format": "semver-requirement"}}
        }),
      )?;
      let accepted = schemas
        .validate(&schema_url, &json!({"version": "1.2.3", "requirement": "^1.2"}))
        .await;
      let rejected = schemas
        .validate(&schema_url, &json!({"version": "release", "requirement": "maybe"}))
        .await;
      Ok::<_, FixtureError>((schemas, accepted, rejected))
    });
    ensure_that(
      observed,
      "custom formats must accept valid semantic versions and reject each invalid value at its own property",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let Ok(ref errors) = actual.2 else {
          return false;
        };
        actual.1.as_ref().is_ok_and(Vec::is_empty)
          && ["version", "requirement"].iter().all(|property| {
            errors
              .iter()
              .any(|error| single_property_path(&error.instance_path) == Some(*property))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  #[test]
  fn local_and_concurrent_schema_families_preserve_validation_and_traversal() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let schema_url = url("https://example.com/execution-families.json")?;
      let schema =
        json!({"type": "object", "properties": {"items": {"type": "array", "items": {"title": "array item", "type": "integer"}}}});
      let instance = json!({"items": [1, "wrong"]});
      let item_path = indexed_keys("items", 0)?;
      let local = Schemas::new_local(TestEnvironment::default(), local_http_client()?)?;
      let concurrent_environment = TestEnvironment::default();
      let concurrent_http = concurrent_http_client(&concurrent_environment, Duration::from_secs(2))?;
      let concurrent = Schemas::new_concurrent(concurrent_environment, concurrent_http)?;
      local.add_schema(&schema_url, Arc::new(schema.clone()));
      concurrent.add_schema(&schema_url, Arc::new(schema.clone()));
      let local_loaded = local.load_schema(&schema_url).await;
      let local_errors = local.validate(&schema_url, &instance).await;
      let concurrent_loaded = concurrent.load_schema_concurrent(&schema_url).await;
      let concurrent_errors = concurrent.validate_concurrent(&schema_url, &instance).await;
      let at_item = concurrent.schemas_at_path_concurrent(&schema_url, &instance, &item_path).await;
      let descendants = concurrent
        .possible_schemas_from_concurrent(&schema_url, &instance, &item_path, 1)
        .await;
      Ok::<_, FixtureError>((
        local,
        concurrent,
        schema,
        [local_loaded, concurrent_loaded],
        [local_errors, concurrent_errors],
        at_item,
        descendants,
      ))
    });
    ensure_that(
      observed,
      "local and concurrent schema families must retain equivalent validation and traversal outcomes",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [Ok(ref local_errors), Ok(ref concurrent_errors)] = actual.4 else {
          return false;
        };
        actual
          .3
          .iter()
          .all(|loaded| loaded.as_ref().is_ok_and(|schema| **schema == actual.2))
          && [local_errors, concurrent_errors].into_iter().all(|errors| {
            errors.iter().any(|error|
              matches!(error.instance_path.as_slice(), [PathSegment::Property(property), PathSegment::Index(1)] if property == "items"))
          })
          && actual.5.as_ref().is_ok_and(|resolved| {
            resolved
              .iter()
              .any(|entry| schema_text(&entry.1, "title") == Some("array item"))
          })
          && actual.6.as_ref().is_ok_and(|children| {
            children
              .iter()
              .any(|child| child.1.is_empty() && schema_text(&child.2, "title") == Some("array item"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn offline_transport_preserves_local_schema_capabilities() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let environment = TestEnvironment::default();
      environment.insert_file("/workspace/file-schema.json", serde_json::to_vec(&json!({"title": "file"}))?);
      let schemas = Schemas::new_offline(environment.clone())?;
      let builtin_url = url(builtins::TAPLO_CONFIG_URL)?;
      let memory_url = url("https://example.com/in-memory.json")?;
      let file_url = url("file:///workspace/file-schema.json")?;
      let disk_url = url("https://example.com/disk.json")?;
      let remote_url = url("https://example.com/not-cached.json")?;
      seed(&schemas, &memory_url, json!({"title": "memory"}));
      let builtin = schemas.load_schema(&builtin_url).await;
      let memory = schemas.load_schema(&memory_url).await;
      let file = schemas.load_schema(&file_url).await;
      schemas.cache().set_cache_path(Some(PathBuf::from("/cache")));
      let saved = schemas.cache().save(disk_url.clone(), Arc::new(json!({"title": "disk"}))).await;
      let reader = Schemas::new_offline(environment);
      let disk_reads = if let Ok(ref fresh) = reader {
        fresh.cache().set_cache_path(Some(PathBuf::from("/cache")));
        Some((
          fresh.load_schema(&disk_url).await,
          fresh.load_schema(&remote_url).await,
          fresh.associations().add_from_catalog(&remote_url).await,
        ))
      } else {
        None
      };
      Ok::<_, FixtureError>((schemas, builtin, memory, file, saved, reader, disk_reads))
    });
    ensure_that(
      observed,
      "offline services must retain builtin, memory, file, and disk schemas while explaining unavailable remote transport",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.is_ok()
          && actual
            .2
            .as_ref()
            .is_ok_and(|schema| schema_text(schema, "title") == Some("memory"))
          && actual
            .3
            .as_ref()
            .is_ok_and(|schema| schema_text(schema, "title") == Some("file"))
          && actual.4.is_ok()
          && actual.6.as_ref().is_some_and(|disk| {
            disk.0.as_ref().is_ok_and(|schema| schema_text(schema, "title") == Some("disk"))
              && disk
                .1
                .as_ref()
                .is_err_and(|error| error.to_string().contains("remote schema transport is unavailable"))
              && disk
                .2
                .as_ref()
                .is_err_and(|error| error.to_string().contains("remote schema transport is unavailable"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn pattern_properties_match_reject_and_report_invalid_regex() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let (schemas, matching_url) = seeded_schemas(
        "https://example.com/pattern.json",
        json!({"patternProperties": {"^foo$": {"title": "matched"}}}),
      )?;
      let invalid_url = url("https://example.com/invalid-pattern.json")?;
      seed(&schemas, &invalid_url, json!({"patternProperties": {"[": {"title": "invalid"}}}));
      let instance = json!({"foo": 1, "bar": 2});
      let foo = keys("foo")?;
      let bar = keys("bar")?;
      let matching = schemas.schemas_at_path(&matching_url, &instance, &foo).await;
      let nonmatching = schemas.schemas_at_path(&matching_url, &instance, &bar).await;
      let invalid = schemas.schemas_at_path(&invalid_url, &instance, &foo).await;
      Ok::<_, FixtureError>((schemas, matching, nonmatching, invalid))
    });
    ensure_that(
      observed,
      "pattern properties must preserve match polarity and invalid regex context",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual
          .1
          .as_ref()
          .is_ok_and(|schemas| schemas.iter().any(|entry| schema_text(&entry.1, "title") == Some("matched")))
          && actual.2.as_ref().is_ok_and(Vec::is_empty)
          && actual.3.as_ref().is_err_and(|error| {
            error.to_string().contains("invalid `patternProperties` expression `[`") && error.to_string().contains("foo")
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn child_schema_traversal_distinguishes_wrappers_regular_composition_and_depth() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let (schemas, wrapper_url) = seeded_schemas(
        "https://example.com/wrapper.json",
        json!({"description": "wrapper docs", "allOf": [
          {"properties": {"left": {"type": "string"}}}, {"properties": {"right": {"type": "integer"}}}, {"$ref": "all-of-branch.json"}
        ]}),
      )?;
      let referenced_url = url("https://example.com/all-of-branch.json")?;
      let regular_url = url("https://example.com/regular.json")?;
      let empty_url = url("https://example.com/empty-composition.json")?;
      seed(
        &schemas,
        &referenced_url,
        json!({"properties": {"referenced": {"type": "boolean"}}}),
      );
      seed(
        &schemas,
        &regular_url,
        json!({"properties": {"own": {"type": "boolean"}}, "allOf": [{"properties": {"branch": {"type": "number"}}}]}),
      );
      seed(&schemas, &empty_url, json!({"title": "empty", "allOf": []}));
      let results = [
        schemas.possible_schemas_from(&wrapper_url, &json!({}), &Keys::empty(), 2).await,
        schemas.possible_schemas_from(&regular_url, &json!({}), &Keys::empty(), 2).await,
        schemas.possible_schemas_from(&empty_url, &json!({}), &Keys::empty(), 0).await,
        schemas.possible_schemas_from(&empty_url, &json!({}), &Keys::empty(), 1).await,
      ];
      Ok::<_, FixtureError>((schemas, results))
    });
    ensure_that(
      observed,
      "child traversal must preserve wrapper metadata, all composition branches, and depth boundaries",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref wrapper, ref regular, ref zero, ref positive] = actual.1;
        wrapper.as_ref().is_ok_and(|children| {
          children
            .iter()
            .any(|child| child.1.is_empty() && schema_text(&child.2, "description") == Some("wrapper docs"))
            && ["left", "right", "referenced"]
              .iter()
              .all(|path| children.iter().any(|child| child.1.dotted() == *path))
        }) && regular.as_ref().is_ok_and(|children| {
          ["own", "branch"]
            .iter()
            .all(|path| children.iter().any(|child| child.1.dotted() == *path))
        }) && zero.as_ref().is_ok_and(Vec::is_empty)
          && positive.as_ref().is_ok_and(|children| {
            children
              .iter()
              .any(|child| child.1.is_empty() && schema_text(&child.2, "title") == Some("empty"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_local_and_invalid_references_have_exact_typed_outcomes() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let schemas = offline_schemas()?;
      let internal_url = url("https://example.com/internal.json")?;
      let invalid_url = url("https://example.com/invalid-reference.json")?;
      seed(
        &schemas,
        &internal_url,
        json!({"$defs": {"leaf": {"title": "internal leaf"}}, "$ref": "#/$defs/leaf"}),
      );
      seed(&schemas, &invalid_url, json!({"$ref": "http://["}));
      let internal = schemas.schemas_at_path(&internal_url, &Value::Null, &Keys::empty()).await;
      let invalid = schemas.schemas_at_path(&invalid_url, &Value::Null, &Keys::empty()).await;
      Ok::<_, FixtureError>((schemas, invalid_url, internal, invalid))
    });
    ensure_that(
      observed,
      "fragment-only references must select their owner and invalid references must retain both owner and rejected value",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.2.as_ref().is_ok_and(|schemas| {
          schemas
            .iter()
            .any(|entry| schema_text(&entry.1, "title") == Some("internal leaf"))
        }) && matches!(&actual.3, Err(SchemaError::InvalidReference { root, reference }) if root == &actual.1 && reference == "http://[")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn relative_references_resolve_against_owning_documents_and_identifiers() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let schemas = offline_schemas()?;
      let root_url = url("https://example.com/root.json")?;
      let first_url = url("https://example.com/schemas/first.json")?;
      let second_url = url("https://example.com/schemas/second.json")?;
      let identifier_root = url("https://example.com/id-root.json")?;
      let identifier_leaf = url("https://example.com/schemas/leaf.json")?;
      let next_path = keys("next")?;
      let leaf_path = keys("leaf")?;
      seed(&schemas, &root_url, json!({"$ref": "schemas/first.json"}));
      seed(
        &schemas,
        &first_url,
        json!({"properties": {"next": {"$ref": "second.json#/$defs/leaf"}}}),
      );
      seed(
        &schemas,
        &second_url,
        json!({"$defs": {"leaf": {"title": "external leaf", "properties": {"child": {"type": "string"}}}}}),
      );
      seed(
        &schemas,
        &identifier_root,
        json!({"$id": "schemas/base.json", "properties": {"leaf": {"$ref": "leaf.json"}}}),
      );
      seed(&schemas, &identifier_leaf, json!({"title": "id-relative leaf"}));
      let resolved = schemas.schemas_at_path(&root_url, &json!({"next": {}}), &next_path).await;
      let descendants = schemas.possible_schemas_from(&root_url, &json!({}), &Keys::empty(), 3).await;
      let identified = schemas
        .schemas_at_path(&identifier_root, &json!({"leaf": null}), &leaf_path)
        .await;
      Ok::<_, FixtureError>((schemas, resolved, descendants, identified))
    });
    ensure_that(
      observed,
      "relative references must follow each owning document and the nearest schema identifier",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.as_ref().is_ok_and(|schemas| {
          schemas
            .iter()
            .any(|entry| schema_text(&entry.1, "title") == Some("external leaf"))
        }) && actual.2.as_ref().is_ok_and(|children| {
          children
            .iter()
            .any(|child| child.1.dotted() == "next" && schema_text(&child.2, "title") == Some("external leaf"))
        }) && actual.3.as_ref().is_ok_and(|schemas| {
          schemas
            .iter()
            .any(|entry| schema_text(&entry.1, "title") == Some("id-relative leaf"))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn reference_cycles_terminate_without_inventing_schemas() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let schemas = offline_schemas()?;
      let root = url("https://example.com/cycle-root.json")?;
      let first = url("https://example.com/a.json")?;
      let second = url("https://example.com/b.json")?;
      seed(&schemas, &root, json!({"$ref": "a.json"}));
      seed(&schemas, &first, json!({"$ref": "b.json"}));
      seed(&schemas, &second, json!({"$ref": "a.json"}));
      let resolved = schemas.schemas_at_path(&root, &Value::Null, &Keys::empty()).await;
      Ok::<_, FixtureError>((schemas, resolved))
    });
    ensure_that(
      observed,
      "a pure reference cycle must terminate without inventing a terminal schema",
      |result| result.as_ref().is_ok_and(|actual| actual.1.as_ref().is_ok_and(Vec::is_empty)),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn borrowed_validation_errors_retain_owned_paths_messages_and_ranges() -> Result<(), impl Debug> {
    let observed = (|| {
      let schema = json!({"type": "object", "additionalProperties": false, "properties": {"known": {"type": "integer"}}});
      let root = parse_valid_dom(
        "known = \"wrong\"\nunexpected = 1\n",
        "the validation range fixture must parse cleanly",
      )?;
      let validator = jsonschema::validator_for(&schema);
      let value = json!({"known": "wrong", "unexpected": 1});
      let owned = validator.as_ref().ok().map(|compiled| {
        compiled
          .iter_errors(&value)
          .map(|error| SchemaValidationError::from_jsonschema(&error))
          .collect::<Vec<_>>()
      });
      let projected = owned.as_ref().map(|errors| {
        errors
          .iter()
          .map(|error| NodeValidationError::new(&root, error.clone()))
          .collect::<Vec<_>>()
      });
      Ok::<_, FixtureError>((schema, root, validator, value, owned, projected))
    })();
    ensure_that(
      observed,
      "borrowed validation errors must preserve owned property paths, messages, and concrete DOM ranges",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let Some(ref errors) = actual.4 else {
          return false;
        };
        errors.iter().any(|error| {
          error
            .additional_properties
            .as_ref()
            .is_some_and(|properties| properties == &["unexpected"])
            && !error.message.is_empty()
        }) && errors.iter().any(|error| {
          error.additional_properties.is_none()
            && matches!(error.instance_path.as_slice(), [PathSegment::Property(property)] if property == "known")
        }) && actual.5.as_ref().is_some_and(|nodes| {
          nodes
            .iter()
            .all(|node| node.as_ref().is_ok_and(|error| !error.text_ranges().is_empty()))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
