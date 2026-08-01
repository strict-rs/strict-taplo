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
    ) -> $future<'schemas, Result<Vec<(Keys, Keys, Arc<Value>)>, SchemaError>>
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
    let additional_properties = match *error.kind() {
      ValidationErrorKind::AdditionalProperties {
        ref unexpected,
      } => Some(unexpected.clone()),
      ValidationErrorKind::AdditionalItems {
        ..
      }
      | ValidationErrorKind::AnyOf {
        ..
      }
      | ValidationErrorKind::BacktrackLimitExceeded {
        ..
      }
      | ValidationErrorKind::RegexEngineFailure {
        ..
      }
      | ValidationErrorKind::Constant {
        ..
      }
      | ValidationErrorKind::Contains
      | ValidationErrorKind::ContentEncoding {
        ..
      }
      | ValidationErrorKind::ContentMediaType {
        ..
      }
      | ValidationErrorKind::Custom {
        ..
      }
      | ValidationErrorKind::Enum {
        ..
      }
      | ValidationErrorKind::ExclusiveMaximum {
        ..
      }
      | ValidationErrorKind::ExclusiveMinimum {
        ..
      }
      | ValidationErrorKind::FalseSchema
      | ValidationErrorKind::Format {
        ..
      }
      | ValidationErrorKind::FromUtf8 {
        ..
      }
      | ValidationErrorKind::MaxItems {
        ..
      }
      | ValidationErrorKind::Maximum {
        ..
      }
      | ValidationErrorKind::MaxLength {
        ..
      }
      | ValidationErrorKind::MaxProperties {
        ..
      }
      | ValidationErrorKind::MinItems {
        ..
      }
      | ValidationErrorKind::Minimum {
        ..
      }
      | ValidationErrorKind::MinLength {
        ..
      }
      | ValidationErrorKind::MinProperties {
        ..
      }
      | ValidationErrorKind::MultipleOf {
        ..
      }
      | ValidationErrorKind::Not {
        ..
      }
      | ValidationErrorKind::OneOfMultipleValid {
        ..
      }
      | ValidationErrorKind::OneOfNotValid {
        ..
      }
      | ValidationErrorKind::Pattern {
        ..
      }
      | ValidationErrorKind::PropertyNames {
        ..
      }
      | ValidationErrorKind::Required {
        ..
      }
      | ValidationErrorKind::Type {
        ..
      }
      | ValidationErrorKind::UnevaluatedItems {
        ..
      }
      | ValidationErrorKind::UnevaluatedProperties {
        ..
      }
      | ValidationErrorKind::UniqueItems
      | ValidationErrorKind::Referencing(_) => None,
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
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;

  use futures::executor::block_on;
  use jsonschema::Retrieve as _;
  use jsonschema::Uri;
  use parking_lot::Mutex;
  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::dom::Keys;
  use taplo::dom::Node;
  use taplo::parser::parse;
  use taplo_test_support::ensure_result;
  use url::Url;

  use super::CacheRetriever;
  use super::NodeValidationError;
  use super::PathSegment;
  use super::SchemaError;
  use super::SchemaValidationError;
  use super::Schemas;
  use super::ValueExt as _;
  use super::builtins;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use super::transport::ConcurrentSchemaTransport;
  #[cfg(feature = "reqwest")]
  use super::transport::LocalSchemaTransport;
  use super::transport::OfflineSchemaTransport;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use super::transport::concurrent_http_client;
  #[cfg(feature = "reqwest")]
  use super::transport::local_http_client;
  use crate::test_support::TestEnvironment;
  /// Offline schema service used by behavior tests.
  type TestSchemas = Schemas<OfflineSchemaTransport<TestEnvironment>>;
  /// Current-thread HTTP-capable schema service used by behavior tests.
  #[cfg(feature = "reqwest")]
  type LocalTestSchemas = Schemas<LocalSchemaTransport<TestEnvironment>>;
  /// Native concurrent schema service used by behavior tests.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  type ConcurrentTestSchemas = Schemas<ConcurrentSchemaTransport<TestEnvironment>>;

  fn url(input: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(input), "the schema fixture URL must parse")
  }

  fn keys(input: &str) -> Result<Keys, TestFailure> {
    ensure_ok(input.parse(), "the schema fixture path must parse")
  }

  /// Construct one typed object-property and array-index path.
  fn indexed_keys(property: &str, index: usize) -> Result<Keys, TestFailure> {
    keys(property).map(|path| path.join(index))
  }

  fn seed(schemas: &TestSchemas, schema_url: &Url, schema: Value) {
    schemas.add_schema(schema_url, Arc::new(schema));
  }

  /// Construct offline schema services for one behavior fixture.
  fn offline_schemas() -> Result<TestSchemas, TestFailure> {
    ensure_result(
      Schemas::new_offline(TestEnvironment::default()),
      "offline schema services must initialize",
    )
  }

  /// Construct current-thread HTTP-capable schema services.
  #[cfg(feature = "reqwest")]
  fn local_schemas(environment: TestEnvironment) -> Result<LocalTestSchemas, TestFailure> {
    let http = ensure_result(local_http_client(), "the local schema HTTP client must initialize")?;
    ensure_result(Schemas::new_local(environment, http), "local schema services must initialize")
  }

  /// Construct native concurrent schema services.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  fn concurrent_schemas(environment: TestEnvironment) -> Result<ConcurrentTestSchemas, TestFailure> {
    let http = ensure_result(
      concurrent_http_client(&environment, Duration::from_secs(2)),
      "the concurrent schema HTTP client must initialize",
    )?;
    ensure_result(
      Schemas::new_concurrent(environment, http),
      "concurrent schema services must initialize",
    )
  }

  /// Construct offline services with one schema already seeded.
  fn seeded_schemas(schema_url: &str, schema: Value) -> Result<(TestSchemas, Url), TestFailure> {
    let schemas = offline_schemas()?;
    let parsed_url = url(schema_url)?;
    seed(&schemas, &parsed_url, schema);
    Ok((schemas, parsed_url))
  }

  /// Construct seeded services around one named object-property schema.
  fn seeded_object_property_schemas(
    schema_url: &str,
    property: &str,
    property_schema: Value,
    additional_properties: Value,
  ) -> Result<(TestSchemas, Url), TestFailure> {
    seeded_schemas(schema_url, object_property_schema(property, property_schema, additional_properties))
  }

  /// Read one textual property from a schema fixture.
  fn schema_text<'a>(schema: &'a Value, name: &str) -> Option<&'a str> {
    schema.get(name).and_then(Value::as_str)
  }

  /// Project descendant paths and one textual metadata field for exact assertions.
  fn child_text_facts(children: &[(Keys, Keys, Arc<Value>)], name: &str) -> Vec<(String, Option<String>)> {
    children
      .iter()
      .map(|child| (child.1.dotted().to_owned(), schema_text(&child.2, name).map(str::to_owned)))
      .collect()
  }

  /// Project a single-property validation path into its stable property name.
  fn single_property_path(path: &[PathSegment]) -> Option<&str> {
    match path {
      [PathSegment::Property(property)] => Some(property),
      [PathSegment::Index(_)] | [] | [_, _, ..] => None,
    }
  }

  /// Build one object schema with a named property and explicit fallback policy.
  fn object_property_schema(property: &str, property_schema: Value, additional_properties: Value) -> Value {
    json!({
      "type": "object",
      "additionalProperties": additional_properties,
      "properties": {
        (property): property_schema
      }
    })
  }

  /// Build one schema branch that contributes a single named property.
  fn property_branch(property: &str, property_schema: Value) -> Value {
    json!({
      "properties": {
        (property): property_schema
      }
    })
  }

  /// Build a two-branch composition fixture from named property contracts.
  fn paired_composition(kind: &str, first: (&str, &str), second: (&str, &str)) -> Value {
    json!({
      (kind): [
        property_branch(first.0, json!({ "title": first.1 })),
        property_branch(second.0, json!({ "title": second.1 }))
      ]
    })
  }

  /// Seed one two-branch composition schema and return its parsed URL.
  fn seed_composition(
    schemas: &TestSchemas,
    schema_url: &str,
    kind: &str,
    first: (&str, &str),
    second: (&str, &str),
  ) -> Result<Url, TestFailure> {
    let parsed_url = url(schema_url)?;
    seed(schemas, &parsed_url, paired_composition(kind, first, second));
    Ok(parsed_url)
  }

  /// Parse one syntax-clean schema-validation DOM fixture.
  fn parse_valid_dom(source: &str, context: &'static str) -> Result<Node, TestFailure> {
    let parsed = ensure_ok(parse(source), context)?;
    ensure(parsed.diagnostics().is_empty(), context)?;
    Ok(parsed.into_dom())
  }

  #[test]
  fn builtins_references_and_validator_retrieval_keep_their_public_contracts() -> Result<(), TestFailure> {
    let generated = ensure_result(
      builtins::taplo_config_schema(),
      "the generated Taplo configuration schema must serialize",
    )?;
    ensure(generated.is_object(), "the generated configuration schema must be an object")?;

    let builtin_url = url(builtins::TAPLO_CONFIG_URL)?;
    let resolved_builtin = ensure_some(
      ensure_result(builtins::builtin_schema(&builtin_url), "the built-in URL must resolve")?,
      "the Taplo configuration URL must have a built-in schema",
    )?;
    ensure(
      *resolved_builtin == *generated,
      "direct generation and URL-based built-in resolution must agree",
    )?;
    let external_url = url("https://example.com/not-built-in.json")?;
    ensure(
      ensure_result(
        builtins::builtin_schema(&external_url),
        "an external URL must be classified normally",
      )?
      .is_none(),
      "an external URL must not be misclassified as a built-in",
    )?;

    let reference = json!({ "$ref": "definitions.json#/$defs/value" });
    ensure(reference.is_schema_ref(), "a string `$ref` must be recognized")?;
    ensure(
      reference.schema_ref() == Some("definitions.json#/$defs/value"),
      "the exact string `$ref` must be exposed",
    )?;
    ensure(
      (json!({ "$ref": 7 }).is_schema_ref(), json!({ "type": "string" }).schema_ref()) == (false, None),
      "non-string and absent `$ref` members must not become references",
    )?;

    let schemas = offline_schemas()?;
    ensure_contains(
      &format!("{schemas:?}"),
      "cached_validators",
      "schema-service debug output must expose stable cache state",
    )?;
    let missing = Arc::new(Mutex::new(Vec::new()));
    let retriever = CacheRetriever {
      store:   schemas.cache().memory_store(),
      missing: Arc::clone(&missing),
    };
    let relative_uri = ensure_some(
      Uri::<String>::parse(String::from("http://[v1.fe80]")).ok(),
      "an RFC URI using an IPvFuture host must parse for validator retrieval",
    )?;
    let relative_error = ensure_some(
      retriever.retrieve(&relative_uri).err(),
      "a URI outside the URL parser's host model cannot identify a cache entry",
    )?;
    ensure_contains(
      &relative_error.to_string(),
      "invalid schema URI",
      "a non-URL validator request must retain its typed URI failure",
    )?;

    let missing_url = url("https://example.com/missing-schema.json")?;
    let missing_uri = ensure_some(
      Uri::<String>::parse(missing_url.to_string()).ok(),
      "an absolute schema URI must parse for validator retrieval",
    )?;
    let missing_error = ensure_some(
      retriever.retrieve(&missing_uri).err(),
      "an uncached absolute schema must be reported missing",
    )?;
    ensure_contains(
      &missing_error.to_string(),
      "is not cached yet",
      "an uncached validator request must retain the requested URL",
    )?;
    ensure(
      *missing.lock() == [missing_url.clone()],
      "validator retrieval must record exactly the absolute schema it needs prefetched",
    )?;

    seed(&schemas, &missing_url, json!({ "title": "cached" }));
    let retrieved = ensure_result(
      retriever.retrieve(&missing_uri),
      "validator retrieval must read a schema after it is prefetched",
    )?;
    ensure(
      schema_text(&retrieved, "title") == Some("cached"),
      "validator retrieval must preserve the cached schema value",
    )
  }

  #[test]
  fn validation_caches_compilers_and_projects_object_and_array_failures() -> Result<(), TestFailure> {
    block_on(async {
      let (schemas, schema_url) = seeded_object_property_schemas(
        "https://example.com/validation.json",
        "values",
        json!({
          "type": "array",
          "items": { "type": "integer" }
        }),
        Value::Bool(false),
      )?;
      let valid = ensure_result(
        schemas.validate(&schema_url, &json!({ "values": [1, 2] })).await,
        "a valid JSON instance must complete validation",
      )?;
      ensure(valid.is_empty(), "a valid JSON instance must produce no errors")?;

      let invalid = ensure_result(
        schemas
          .validate(&schema_url, &json!({ "values": [1, "wrong"], "unexpected": true }))
          .await,
        "an invalid JSON instance must return owned validation errors",
      )?;
      ensure(
        invalid.iter().any(|error| {
          matches!(
            error.instance_path.as_slice(),
            [PathSegment::Property(property), PathSegment::Index(1)] if property == "values"
          )
        }),
        "an array-item failure must retain its object property and array index",
      )?;
      ensure(
        invalid.iter().any(|error| {
          error
            .additional_properties
            .as_ref()
            .is_some_and(|properties| properties == &["unexpected"])
        }),
        "an unexpected property failure must retain the rejected property",
      )?;

      let first_validator = ensure_some(
        ensure_result(
          schemas.get_validator(&schema_url),
          "the compiled validator cache must remain readable",
        )?,
        "successful validation must install a compiled validator",
      )?;
      let repeated = ensure_result(
        schemas.validate(&schema_url, &json!({ "values": [] })).await,
        "repeated validation must use the installed validator",
      )?;
      ensure(repeated.is_empty(), "the reused validator must preserve successful behavior")?;
      let second_validator = ensure_some(
        ensure_result(
          schemas.get_validator(&schema_url),
          "the reused validator cache must remain readable",
        )?,
        "repeated validation must leave a compiled validator installed",
      )?;
      ensure(
        Arc::ptr_eq(&first_validator, &second_validator),
        "repeated validation must reuse the same compiled validator",
      )?;

      let root = parse_valid_dom(
        "values = [1, \"wrong\"]\nunexpected = true\n",
        "the DOM validation fixture must parse without syntax diagnostics",
      )?;
      let projected = ensure_result(
        schemas.validate_root(&schema_url, &root).await,
        "DOM validation must project JSON errors back to source nodes",
      )?;
      let array_path = indexed_keys("values", 1)?;
      let array_error = ensure_some(
        projected.iter().find(|error| error.keys == array_path),
        "the array-item error must resolve to the indexed TOML node",
      )?;
      ensure(
        !array_error.text_ranges().is_empty(),
        "the indexed TOML error must retain a concrete source range",
      )?;
      let additional_error = ensure_some(
        projected.iter().find(|error| error.additional_properties),
        "the unexpected TOML key must resolve as an additional-property error",
      )?;
      ensure(
        !additional_error.text_ranges().is_empty(),
        "the unexpected TOML key must retain a concrete source range",
      )?;

      let malformed = ensure_ok(
        parse("values = 999999999999999999999999999999\n"),
        "the malformed scalar validation fixture must still produce a parse result",
      )?
      .into_dom();
      ensure(
        matches!(
          schemas.validate_root(&schema_url, &malformed).await,
          Err(SchemaError::DomSerialization { .. })
        ),
        "a semantic DOM that cannot become JSON must fail with the typed serialization error",
      )?;

      ensure_result(
        schemas.cache().set_expiration_times(Duration::ZERO, Duration::from_secs(60)),
        "the validator expiration policy must install",
      )?;
      ensure(
        ensure_result(schemas.get_validator(&schema_url), "expired validator state must be evaluated")?.is_none(),
        "an expired schema generation must clear compiled validators",
      )
    })
  }

  #[test]
  fn schema_paths_distinguish_tuple_indices_fallbacks_and_composition_branches() -> Result<(), TestFailure> {
    block_on(async {
      let (schemas, indexed_url) = seeded_object_property_schemas(
        "https://example.com/indexed.json",
        "values",
        json!({
          "items": [
            { "title": "first" },
            { "title": "second" }
          ]
        }),
        json!({ "title": "fallback" }),
      )?;
      let instance = json!({ "values": [true, 7], "other": "text" });
      let second = ensure_result(
        schemas
          .schemas_at_path(&indexed_url, &instance, &indexed_keys("values", 1)?)
          .await,
        "an in-range tuple index must resolve",
      )?;
      ensure(
        second
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("second")),
        "an in-range tuple index must select its positional schema",
      )?;
      ensure(
        !second.iter().any(|resolved| schema_text(&resolved.1, "title") == Some("first")),
        "an in-range tuple index must not select a sibling positional schema",
      )?;
      let out_of_range = ensure_result(
        schemas
          .schemas_at_path(&indexed_url, &instance, &indexed_keys("values", 4)?)
          .await,
        "an out-of-range tuple index must terminate normally",
      )?;
      ensure(
        out_of_range.is_empty(),
        "an out-of-range tuple index must not invent a positional schema",
      )?;
      let fallback = ensure_result(
        schemas.schemas_at_path(&indexed_url, &instance, &keys("other")?).await,
        "an undeclared object property must resolve through `additionalProperties`",
      )?;
      ensure(
        fallback
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("fallback")),
        "an undeclared object property must retain the fallback schema",
      )?;

      let composition_url = seed_composition(
        &schemas,
        "https://example.com/composition.json",
        "oneOf",
        ("choice", "one-of text"),
        ("choice", "one-of number"),
      )?;
      let choices = ensure_result(
        schemas
          .schemas_at_path(&composition_url, &json!({ "choice": null }), &keys("choice")?)
          .await,
        "a composition-only `oneOf` wrapper must expose each viable branch",
      )?;
      let choice_titles = choices
        .iter()
        .filter_map(|resolved| schema_text(&resolved.1, "title"))
        .collect::<Vec<_>>();
      ensure(
        (choice_titles.contains(&"one-of text"), choice_titles.contains(&"one-of number")) == (true, true),
        "both `oneOf` property schemas must survive path resolution",
      )?;

      let alternatives_url = seed_composition(
        &schemas,
        "https://example.com/alternatives.json",
        "anyOf",
        ("left", "left alternative"),
        ("right", "right alternative"),
      )?;
      let descendants = ensure_result(
        schemas
          .possible_schemas_from(&alternatives_url, &json!({}), &Keys::empty(), 2)
          .await,
        "a composition-only `anyOf` wrapper must expose descendant properties",
      )?;
      let descendant_titles = child_text_facts(&descendants, "title");
      ensure(
        (
          descendant_titles.contains(&(String::from("left"), Some(String::from("left alternative")))),
          descendant_titles.contains(&(String::from("right"), Some(String::from("right alternative")))),
        ) == (true, true),
        "both `anyOf` descendants must remain independently discoverable",
      )
    })
  }

  #[test]
  fn external_references_invalid_schemas_and_stale_cache_have_typed_outcomes() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      environment.insert_file(
        "/workspace/referenced.json",
        serde_json::to_vec(&json!({ "type": "string", "minLength": 2 })).unwrap_or_default(),
      );
      let schemas = ensure_result(
        Schemas::new_offline(environment.clone()),
        "the reference-loading schema service must initialize",
      )?;
      let root_url = url("https://example.com/reference-root.json")?;
      seed(&schemas, &root_url, json!({ "$ref": "file:///workspace/referenced.json" }));
      let valid = ensure_result(
        schemas.validate(&root_url, &json!("valid")).await,
        "an external file reference must be prefetched before validator compilation",
      )?;
      ensure(valid.is_empty(), "a value accepted by the external schema must validate")?;
      let invalid = ensure_result(
        schemas.validate(&root_url, &json!("x")).await,
        "an external schema rejection must return owned validation errors",
      )?;
      ensure(
        invalid.iter().any(|error| error.message.contains("shorter than")),
        "the external schema's constraint must remain active after prefetch",
      )?;

      let missing_reference_url = url("https://example.com/missing-reference-root.json")?;
      seed(
        &schemas,
        &missing_reference_url,
        json!({ "$ref": "https://example.com/unavailable.json" }),
      );
      ensure(
        matches!(
          schemas.validate(&missing_reference_url, &Value::Null).await,
          Err(SchemaError::Unavailable { .. })
        ),
        "an unavailable external reference must preserve the typed load failure",
      )?;

      let invalid_schema_url = url("https://example.com/invalid-schema.json")?;
      seed(&schemas, &invalid_schema_url, json!({ "type": 7 }));
      let invalid_schema = ensure_some(
        schemas.validate(&invalid_schema_url, &Value::Null).await.err(),
        "a structurally invalid schema must fail compilation",
      )?;
      ensure(
        matches!(invalid_schema, SchemaError::InvalidSchema { .. }),
        "schema compilation failures must retain the typed invalid-schema variant",
      )?;

      let fragment_url = url("https://example.com/fragments.json")?;
      seed(
        &schemas,
        &fragment_url,
        json!({
          "$defs": {
            "leaf": { "title": "fragment leaf" }
          }
        }),
      );
      let selected_url = url("https://example.com/fragments.json#/$defs/leaf")?;
      let selected = ensure_result(
        schemas.resolve_schema(selected_url).await,
        "an existing JSON-pointer fragment must resolve",
      )?;
      ensure(
        schema_text(&selected, "title") == Some("fragment leaf"),
        "fragment resolution must return only the selected schema",
      )?;
      let missing_fragment_url = url("https://example.com/fragments.json#/$defs/missing")?;
      ensure(
        matches!(
          schemas.resolve_schema(missing_fragment_url).await,
          Err(SchemaError::MissingFragment { fragment, .. }) if fragment == "/$defs/missing"
        ),
        "a missing JSON-pointer fragment must retain its exact typed context",
      )?;

      let stale_url = url("https://example.com/stale.json")?;
      schemas.cache().set_cache_path(Some(PathBuf::from("/cache")));
      ensure_result(
        schemas
          .cache()
          .set_expiration_times(Duration::from_secs(1), Duration::from_secs(1)),
        "the short stale-cache policy must install",
      )?;
      ensure_result(
        schemas
          .cache()
          .save(stale_url.clone(), Arc::new(json!({ "title": "stale fallback" })))
          .await,
        "the stale schema fixture must persist",
      )?;
      let deadline = ensure_some(
        time::OffsetDateTime::UNIX_EPOCH.checked_add(time::Duration::seconds(1)),
        "the stale-cache deadline must be representable",
      )?;
      environment.set_now(deadline);
      let stale_reader = ensure_result(Schemas::new_offline(environment), "the stale-cache reader must initialize")?;
      stale_reader.cache().set_cache_path(Some(PathBuf::from("/cache")));
      let stale = ensure_result(
        stale_reader.load_schema(&stale_url).await,
        "an expired disk entry must recover an unavailable remote schema",
      )?;
      ensure(
        schema_text(&stale, "title") == Some("stale fallback"),
        "stale recovery must preserve the complete cached schema",
      )
    })
  }

  #[test]
  fn custom_semver_formats_accept_valid_values_and_reject_invalid_values() -> Result<(), TestFailure> {
    block_on(async {
      let (schemas, schema_url) = seeded_schemas(
        "https://example.com/semantic-versions.json",
        json!({
          "type": "object",
          "required": ["version", "requirement"],
          "properties": {
            "version": {
              "type": "string",
              "format": "semver"
            },
            "requirement": {
              "type": "string",
              "format": "semver-requirement"
            }
          }
        }),
      )?;
      let accepted = ensure_result(
        schemas
          .validate(&schema_url, &json!({ "version": "1.2.3", "requirement": "^1.2" }))
          .await,
        "valid semantic-version strings must complete validation",
      )?;
      ensure(accepted.is_empty(), "valid semantic versions and requirements must be accepted")?;
      let rejected = ensure_result(
        schemas
          .validate(&schema_url, &json!({ "version": "release", "requirement": "maybe" }))
          .await,
        "invalid semantic-version strings must return validation errors",
      )?;
      let rejected_paths = rejected
        .iter()
        .filter_map(|error| single_property_path(&error.instance_path))
        .collect::<Vec<_>>();
      ensure(
        (rejected_paths.contains(&"version"), rejected_paths.contains(&"requirement")) == (true, true),
        "each invalid custom-format value must be rejected at its own property path",
      )
    })
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  #[test]
  fn local_and_concurrent_schema_families_preserve_validation_and_traversal() -> Result<(), TestFailure> {
    block_on(async {
      let schema_url = url("https://example.com/execution-families.json")?;
      let schema = json!({
        "type": "object",
        "properties": {
          "items": {
            "type": "array",
            "items": {
              "title": "array item",
              "type": "integer"
            }
          }
        }
      });
      let instance = json!({ "items": [1, "wrong"] });

      let local = local_schemas(TestEnvironment::default())?;
      local.add_schema(&schema_url, Arc::new(schema.clone()));
      let local_loaded = ensure_result(
        local.load_schema(&schema_url).await,
        "the local family must load an in-memory schema",
      )?;
      ensure(
        *local_loaded == schema,
        "the local family must preserve the complete schema document",
      )?;
      let local_errors = ensure_result(
        local.validate(&schema_url, &instance).await,
        "the local family must validate through its local future boundary",
      )?;
      ensure(
        local_errors.iter().any(|error| {
          matches!(
            error.instance_path.as_slice(),
            [PathSegment::Property(property), PathSegment::Index(1)] if property == "items"
          )
        }),
        "the local family must retain the indexed validation failure",
      )?;

      let concurrent = concurrent_schemas(TestEnvironment::default())?;
      concurrent.add_schema(&schema_url, Arc::new(schema));
      let concurrent_loaded = ensure_result(
        concurrent.load_schema_concurrent(&schema_url).await,
        "the concurrent family must load an in-memory schema",
      )?;
      ensure(
        schema_text(&concurrent_loaded, "type") == Some("object"),
        "the concurrent family must preserve the schema root",
      )?;
      let concurrent_errors = ensure_result(
        concurrent.validate_concurrent(&schema_url, &instance).await,
        "the concurrent family must validate through its `Send` future boundary",
      )?;
      ensure(
        concurrent_errors.iter().any(|error| {
          matches!(
            error.instance_path.as_slice(),
            [PathSegment::Property(property), PathSegment::Index(1)] if property == "items"
          )
        }),
        "the concurrent family must retain the same indexed validation failure",
      )?;

      let item_path = indexed_keys("items", 0)?;
      let at_item = ensure_result(
        concurrent.schemas_at_path_concurrent(&schema_url, &instance, &item_path).await,
        "the concurrent family must resolve an array-item schema",
      )?;
      ensure(
        at_item
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("array item")),
        "the concurrent family must retain array-item schema metadata",
      )?;
      let descendants = ensure_result(
        concurrent
          .possible_schemas_from_concurrent(&schema_url, &instance, &item_path, 1)
          .await,
        "the concurrent family must enumerate descendant schemas",
      )?;
      let descendant_titles = child_text_facts(&descendants, "title");
      ensure(
        descendant_titles.contains(&(String::new(), Some(String::from("array item")))),
        "the concurrent descendant query must expose the array-item schema",
      )
    })
  }

  #[test]
  fn offline_transport_preserves_local_schema_capabilities() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      environment.insert_file(
        "/workspace/file-schema.json",
        serde_json::to_vec(&json!({ "title": "file" })).unwrap_or_default(),
      );
      let schemas = ensure_result(Schemas::new_offline(environment.clone()), "offline schema services must initialize")?;

      let builtin_url = url(builtins::TAPLO_CONFIG_URL)?;
      drop(ensure_result(
        schemas.load_schema(&builtin_url).await,
        "offline mode must retain built-in schemas",
      )?);

      let memory_url = url("https://example.com/in-memory.json")?;
      seed(&schemas, &memory_url, json!({ "title": "memory" }));
      let memory = ensure_result(
        schemas.load_schema(&memory_url).await,
        "offline mode must retain explicitly seeded memory schemas",
      )?;
      ensure(schema_text(&memory, "title") == Some("memory"), "memory schema contents")?;

      let file_url = url("file:///workspace/file-schema.json")?;
      let file = ensure_result(schemas.load_schema(&file_url).await, "offline mode must retain file-backed schemas")?;
      ensure(schema_text(&file, "title") == Some("file"), "file schema contents")?;

      let disk_url = url("https://example.com/disk.json")?;
      schemas.cache().set_cache_path(Some(PathBuf::from("/cache")));
      ensure_result(
        schemas
          .cache()
          .save(disk_url.clone(), Arc::new(json!({ "title": "disk" })))
          .await,
        "the disk schema fixture must persist",
      )?;
      let disk_reader = ensure_result(
        Schemas::new_offline(environment),
        "the second offline schema service must initialize",
      )?;
      disk_reader.cache().set_cache_path(Some(PathBuf::from("/cache")));
      let disk = ensure_result(
        disk_reader.load_schema(&disk_url).await,
        "offline mode must retain disk-cached schemas",
      )?;
      ensure(schema_text(&disk, "title") == Some("disk"), "disk schema contents")?;

      let remote_url = url("https://example.com/not-cached.json")?;
      let remote_error = ensure_some(
        disk_reader.load_schema(&remote_url).await.err(),
        "offline remote schema loading must fail",
      )?;
      ensure_contains(
        &remote_error.to_string(),
        "remote schema transport is unavailable",
        "offline schema errors must explain the missing transport",
      )?;
      let catalog_error = ensure_some(
        disk_reader.associations().add_from_catalog(&remote_url).await.err(),
        "offline remote catalog loading must fail",
      )?;
      ensure_contains(
        &catalog_error.to_string(),
        "remote schema transport is unavailable",
        "offline catalog errors must explain the missing transport",
      )
    })
  }

  #[test]
  fn pattern_properties_match_reject_and_report_invalid_regex() -> Result<(), TestFailure> {
    block_on(async {
      let (schemas, matching_url) = seeded_schemas(
        "https://example.com/pattern.json",
        json!({
            "patternProperties": {
                "^foo$": { "title": "matched" }
            }
        }),
      )?;
      let instance = json!({ "foo": 1, "bar": 2 });
      let matching = ensure_result(
        schemas.schemas_at_path(&matching_url, &instance, &keys("foo")?).await,
        "a valid matching pattern must resolve",
      )?;
      ensure(
        matching
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("matched")),
        "a matching pattern property must contribute its schema",
      )?;
      let nonmatching = ensure_result(
        schemas.schemas_at_path(&matching_url, &instance, &keys("bar")?).await,
        "a valid nonmatching pattern must be skipped normally",
      )?;
      ensure(nonmatching.is_empty(), "a valid nonmatching pattern must not contribute a schema")?;

      let invalid_url = url("https://example.com/invalid-pattern.json")?;
      seed(
        &schemas,
        &invalid_url,
        json!({ "patternProperties": { "[": { "title": "invalid" } } }),
      );
      let invalid = ensure_some(
        schemas.schemas_at_path(&invalid_url, &instance, &keys("foo")?).await.err(),
        "an invalid pattern must return a resolution error",
      )?;
      ensure_contains(
        &invalid.to_string(),
        "invalid `patternProperties` expression `[`",
        "the invalid pattern error must include the offending expression",
      )?;
      ensure_contains(
        &invalid.to_string(),
        "foo",
        "the invalid pattern error must include the current key context",
      )
    })
  }

  #[test]
  fn child_schema_traversal_distinguishes_wrappers_regular_composition_and_depth() -> Result<(), TestFailure> {
    block_on(async {
      let (schemas, wrapper_url) = seeded_schemas(
        "https://example.com/wrapper.json",
        json!({
            "description": "wrapper docs",
            "allOf": [
                { "properties": { "left": { "type": "string" } } },
                { "properties": { "right": { "type": "integer" } } },
                { "$ref": "all-of-branch.json" }
            ]
        }),
      )?;
      let referenced_branch_url = url("https://example.com/all-of-branch.json")?;
      seed(
        &schemas,
        &referenced_branch_url,
        json!({ "properties": { "referenced": { "type": "boolean" } } }),
      );
      let wrapper = ensure_result(
        schemas.possible_schemas_from(&wrapper_url, &json!({}), &Keys::empty(), 2).await,
        "a composition-only allOf wrapper must traverse",
      )?;
      let wrapper_descriptions = child_text_facts(&wrapper, "description");
      ensure(
        wrapper_descriptions.contains(&(String::new(), Some(String::from("wrapper docs")))),
        "wrapper metadata must override and survive the merged composition",
      )?;
      let wrapper_paths = wrapper.iter().map(|child| child.1.dotted().to_owned()).collect::<Vec<_>>();
      ensure(
        (
          wrapper_paths.contains(&String::from("left")),
          wrapper_paths.contains(&String::from("right")),
          wrapper_paths.contains(&String::from("referenced")),
        ) == (true, true, true),
        "inline and referenced allOf branch properties must survive the wrapper merge",
      )?;

      let regular_url = url("https://example.com/regular.json")?;
      seed(
        &schemas,
        &regular_url,
        json!({
            "properties": { "own": { "type": "boolean" } },
            "allOf": [
                { "properties": { "branch": { "type": "number" } } }
            ]
        }),
      );
      let regular = ensure_result(
        schemas.possible_schemas_from(&regular_url, &json!({}), &Keys::empty(), 2).await,
        "a regular schema with allOf must traverse independently",
      )?;
      let regular_paths = regular.iter().map(|child| child.1.dotted().to_owned()).collect::<Vec<_>>();
      ensure(
        (
          regular_paths.contains(&String::from("own")),
          regular_paths.contains(&String::from("branch")),
        ) == (true, true),
        "regular own properties and allOf branch properties must both be exposed",
      )?;

      let empty_url = url("https://example.com/empty-composition.json")?;
      seed(&schemas, &empty_url, json!({ "title": "empty", "allOf": [] }));
      let zero_depth = ensure_result(
        schemas.possible_schemas_from(&empty_url, &json!({}), &Keys::empty(), 0).await,
        "zero-depth traversal must terminate normally",
      )?;
      ensure(zero_depth.is_empty(), "zero depth must return no child schemas")?;
      let positive_depth = ensure_result(
        schemas.possible_schemas_from(&empty_url, &json!({}), &Keys::empty(), 1).await,
        "an empty composition array must not suppress the schema",
      )?;
      let positive_titles = child_text_facts(&positive_depth, "title");
      ensure(
        positive_titles.contains(&(String::new(), Some(String::from("empty")))),
        "empty allOf must retain the schema itself at positive depth",
      )
    })
  }

  #[test]
  fn relative_reference_origins_ids_and_cycles_are_deterministic() -> Result<(), TestFailure> {
    block_on(async {
      let schemas = offline_schemas()?;

      let internal_url = url("https://example.com/internal.json")?;
      seed(
        &schemas,
        &internal_url,
        json!({
          "$defs": {
            "leaf": { "title": "internal leaf" }
          },
          "$ref": "#/$defs/leaf"
        }),
      );
      let internal = ensure_result(
        schemas.schemas_at_path(&internal_url, &Value::Null, &Keys::empty()).await,
        "a document-local fragment reference must resolve",
      )?;
      ensure(
        internal
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("internal leaf")),
        "a fragment-only reference must retain its owning document and select the exact definition",
      )?;

      let invalid_reference_url = url("https://example.com/invalid-reference.json")?;
      seed(&schemas, &invalid_reference_url, json!({ "$ref": "http://[" }));
      let invalid_reference = ensure_some(
        schemas
          .schemas_at_path(&invalid_reference_url, &Value::Null, &Keys::empty())
          .await
          .err(),
        "an invalid schema reference must return a typed resolution failure",
      )?;
      ensure(
        matches!(
          invalid_reference,
          SchemaError::InvalidReference {
            ref root,
            ref reference,
          } if root == &invalid_reference_url && reference == "http://["
        ),
        "invalid-reference failure must retain both its owning document and rejected reference",
      )?;

      let root_url = url("https://example.com/root.json")?;
      let first_url = url("https://example.com/schemas/first.json")?;
      let second_url = url("https://example.com/schemas/second.json")?;
      seed(&schemas, &root_url, json!({ "$ref": "schemas/first.json" }));
      seed(
        &schemas,
        &first_url,
        json!({
          "properties": {
            "next": { "$ref": "second.json#/$defs/leaf" }
          }
        }),
      );
      seed(
        &schemas,
        &second_url,
        json!({
          "$defs": {
            "leaf": {
              "title": "external leaf",
              "properties": {
                "child": { "type": "string" }
              }
            }
          }
        }),
      );

      let at_path = ensure_result(
        schemas.schemas_at_path(&root_url, &json!({ "next": {} }), &keys("next")?).await,
        "nested relative references must resolve from their owning document",
      )?;
      ensure(
        at_path
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("external leaf")),
        "a nested relative reference must not fall back to the original root URL",
      )?;
      let descendants = ensure_result(
        schemas.possible_schemas_from(&root_url, &json!({}), &Keys::empty(), 3).await,
        "descendant traversal must retain external reference origins",
      )?;
      let descendant_titles = child_text_facts(&descendants, "title");
      ensure(
        descendant_titles.contains(&(String::from("next"), Some(String::from("external leaf")))),
        "descendant traversal must resolve a referenced child's own relative reference",
      )?;

      let identifier_root = url("https://example.com/id-root.json")?;
      let identifier_leaf = url("https://example.com/schemas/leaf.json")?;
      seed(
        &schemas,
        &identifier_root,
        json!({
          "$id": "schemas/base.json",
          "properties": {
            "leaf": { "$ref": "leaf.json" }
          }
        }),
      );
      seed(&schemas, &identifier_leaf, json!({ "title": "id-relative leaf" }));
      let identified = ensure_result(
        schemas
          .schemas_at_path(&identifier_root, &json!({ "leaf": null }), &keys("leaf")?)
          .await,
        "a schema identifier must replace the inherited relative-reference base",
      )?;
      ensure(
        identified
          .iter()
          .any(|resolved| schema_text(&resolved.1, "title") == Some("id-relative leaf")),
        "a relative reference must resolve against the nearest schema identifier",
      )?;

      let cycle_root = url("https://example.com/cycle-root.json")?;
      let cycle_a = url("https://example.com/a.json")?;
      let cycle_b = url("https://example.com/b.json")?;
      seed(&schemas, &cycle_root, json!({ "$ref": "a.json" }));
      seed(&schemas, &cycle_a, json!({ "$ref": "b.json" }));
      seed(&schemas, &cycle_b, json!({ "$ref": "a.json" }));
      let cycle = ensure_result(
        schemas.schemas_at_path(&cycle_root, &Value::Null, &Keys::empty()).await,
        "a reference cycle must terminate deterministically",
      )?;
      ensure(cycle.is_empty(), "a pure reference cycle must not invent a terminal schema result")
    })
  }

  #[test]
  fn borrowed_validation_errors_retain_owned_paths_messages_and_ranges() -> Result<(), TestFailure> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "known": { "type": "integer" } }
    });
    let validator = ensure_ok(jsonschema::validator_for(&schema), "the validation fixture schema must compile")?;
    let value = json!({ "known": "wrong", "unexpected": 1 });
    let owned: Vec<SchemaValidationError> = validator
      .iter_errors(&value)
      .map(|error| SchemaValidationError::from_jsonschema(&error))
      .collect();

    let additional = ensure_some(
      owned.iter().find(|error| error.additional_properties.is_some()),
      "the additional-properties error must be retained",
    )?;
    ensure(
      additional
        .additional_properties
        .as_ref()
        .is_some_and(|properties| properties == &["unexpected"]),
      "the owned error must retain the unexpected property name",
    )?;
    ensure(
      !additional.message.is_empty(),
      "the owned additional-properties error must retain its message",
    )?;

    let typed = ensure_some(
      owned.iter().find(|error| error.additional_properties.is_none()),
      "the property-type error must be retained",
    )?;
    ensure(
      matches!(typed.instance_path.as_slice(), [PathSegment::Property(property)] if property == "known"),
      "the owned property error must retain its instance path",
    )?;

    let root = parse_valid_dom(
      "known = \"wrong\"\nunexpected = 1\n",
      "the validation range fixture must parse without syntax diagnostics",
    )?;
    let additional_node = ensure_result(
      NodeValidationError::new(&root, additional.clone()),
      "the additional-properties error must resolve to the DOM",
    )?;
    let additional_ranges = additional_node.text_ranges();
    ensure(
      !additional_ranges.is_empty(),
      "an unexpected property must retain its concrete DOM range",
    )?;
    let typed_node = ensure_result(
      NodeValidationError::new(&root, typed.clone()),
      "the property-type error must resolve to the DOM child",
    )?;
    ensure(
      !typed_node.text_ranges().is_empty(),
      "a normal child validation error must retain a child range",
    )
  }
}
