use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use async_recursion::async_recursion;
use itertools::Itertools;
use json_value_merge::Merge;
use jsonschema::Retrieve;
use jsonschema::Validator;
use jsonschema::error::ValidationErrorKind;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;
use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::dom::node::Key;
use taplo::dom::{
  self,
};
use taplo::rowan::TextRange;
use tokio::sync::Semaphore;
use url::Url;

use self::associations::SchemaAssociations;
use self::builtins::builtin_schema;
use self::cache::Cache;
use crate::LruCache;
use crate::environment::Environment;
use crate::util::ArcHashValue;

pub mod associations;
pub mod cache;
pub mod ext;

/// Mutable output and immutable root data shared by recursive child-schema traversal.
struct ChildSchemaContext<'a> {
  /// URL used to resolve schema references.
  root_url:  &'a Url,
  /// Document path at which traversal began.
  root_path: &'a Keys,
  /// Collected absolute path, relative path, and schema triples.
  schemas:   &'a mut Vec<(Keys, Keys, Arc<Value>)>,
}

/// A JSON Schema composition keyword with nonempty branches.
#[derive(Clone, Copy, Eq, PartialEq)]
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
    schema[name].as_array().map_or(&[], Vec::as_slice)
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

pub mod builtins {
  use std::sync::Arc;

  use serde_json::Value;
  use url::Url;

  pub const TAPLO_CONFIG_URL: &str = "taplo://taplo.toml";

  #[must_use]
  pub fn taplo_config_schema() -> Arc<Value> {
    Arc::new(serde_json::to_value(schemars::schema_for!(crate::config::Config)).unwrap())
  }

  #[must_use]
  pub fn builtin_schema(url: &Url) -> Option<Arc<Value>> {
    if url.as_str() == TAPLO_CONFIG_URL {
      Some(taplo_config_schema())
    } else {
      None
    }
  }
}

#[derive(Clone)]
pub struct Schemas<E: Environment> {
  env:                 E,
  associations:        SchemaAssociations<E>,
  concurrent_requests: Arc<Semaphore>,
  http:                Option<reqwest::Client>,
  validators:          Arc<Mutex<LruCache<Url, Arc<Validator>>>>,
  cache:               Cache<E>,
}

impl<E: Environment> Schemas<E> {
  pub fn new(env: E, http: reqwest::Client) -> Self {
    Self::with_http(env, Some(http))
  }

  /// Construct schema services without HTTP/HTTPS transport.
  ///
  /// Built-in, memory-cached, disk-cached, and `file:` schemas remain available. Attempts to
  /// fetch remote schemas or catalogs return an actionable transport error.
  #[must_use]
  pub fn new_offline(env: E) -> Self {
    Self::with_http(env, None)
  }

  /// Construct schema services with an explicit remote-transport capability.
  fn with_http(env: E, http: Option<reqwest::Client>) -> Self {
    let cache = Cache::new(env.clone());

    Self {
      associations: SchemaAssociations::new(env.clone(), cache.clone(), http.clone()),
      cache,
      env,
      concurrent_requests: Arc::new(Semaphore::new(10)),
      http,
      validators: Arc::new(Mutex::new(LruCache::with_hasher(
        NonZeroUsize::new(3).unwrap_or(NonZeroUsize::MIN),
        RandomState::new(),
      ))),
    }
  }

  /// Get a reference to the schemas's associations.
  pub fn associations(&self) -> &SchemaAssociations<E> {
    &self.associations
  }

  /// Get a reference to the schemas's cache.
  pub fn cache(&self) -> &Cache<E> {
    &self.cache
  }

  pub fn env(&self) -> &E {
    &self.env
  }
}

impl<E: Environment> Schemas<E> {
  #[tracing::instrument(skip_all, fields(%schema_url))]
  pub async fn validate_root(&self, schema_url: &Url, root: &dom::Node) -> Result<Vec<NodeValidationError>, anyhow::Error> {
    let value = serde_json::to_value(root)?;
    self
      .validate(schema_url, &value)
      .await?
      .into_iter()
      .map(|error| NodeValidationError::new(root, error))
      .collect::<Result<Vec<_>, _>>()
  }

  #[tracing::instrument(skip_all, fields(%schema_url))]
  pub async fn validate(&self, schema_url: &Url, value: &Value) -> Result<Vec<SchemaValidationError>, anyhow::Error> {
    // External `$ref`s are resolved eagerly when the validator is built
    // (see `create_validator`), so validation itself is a pure, synchronous pass.
    let validator = self.get_or_build_validator(schema_url).await?;
    Ok(
      validator
        .iter_errors(value)
        .map(|error| SchemaValidationError::from_jsonschema(&error))
        .collect(),
    )
  }

  async fn get_or_build_validator(&self, schema_url: &Url) -> Result<Arc<Validator>, anyhow::Error> {
    if let Some(validator) = self.get_validator(schema_url) {
      return Ok(validator);
    }

    let schema = self
      .load_schema(schema_url)
      .await
      .with_context(|| format!("failed to load schema {schema_url}"))?;
    self.add_schema(schema_url, schema.clone()).await;
    self
      .add_validator(schema_url.clone(), &schema)
      .await
      .with_context(|| format!("invalid schema {schema_url}"))
  }

  pub async fn add_schema(&self, schema_url: &Url, schema: Arc<Value>) {
    drop(self.cache.store(schema_url.clone(), schema).await);
  }

  #[tracing::instrument(skip_all, fields(%schema_url))]
  pub async fn load_schema(&self, schema_url: &Url) -> Result<Arc<Value>, anyhow::Error> {
    if let Ok(s) = self.cache.load(schema_url, false).await {
      tracing::debug!(%schema_url, "schema was found in cache");
      return Ok(s);
    }

    let schema = if let Some(builtin) = builtin_schema(schema_url) {
      builtin
    } else {
      match self.fetch_external(schema_url).await {
        Ok(s) => Arc::new(s),
        Err(error) => {
          tracing::warn!(%error, "failed to fetch schema");
          if let Ok(s) = self.cache.load(schema_url, true).await {
            tracing::debug!(%schema_url, "expired schema was found in cache");
            return Ok(s);
          }
          return Err(error);
        }
      }
    };

    if let Err(error) = self.cache.store(schema_url.clone(), schema.clone()).await {
      tracing::debug!(%error, "failed to cache schema");
    }

    Ok(schema)
  }

  fn get_validator(&self, schema_url: &Url) -> Option<Arc<Validator>> {
    if self.cache().lru_expired() {
      self.validators.lock().clear();
    }

    self.validators.lock().get(schema_url).cloned()
  }

  async fn add_validator(&self, schema_url: Url, schema: &Value) -> Result<Arc<Validator>, anyhow::Error> {
    let v = Arc::new(self.create_validator(schema).await?);
    self.validators.lock().put(schema_url, v.clone());
    Ok(v)
  }

  #[async_recursion(?Send)]
  #[must_use]
  pub(crate) async fn resolve_schema(&self, url: Url) -> Result<Arc<Value>, anyhow::Error> {
    match url.fragment() {
      Some(fragment) => {
        let mut res_url = url.clone();
        res_url.set_fragment(None);
        let schema = self.resolve_schema(res_url).await?;
        let ptr = String::from("/") + fragment;
        schema
          .pointer(&ptr)
          .map(|v| Arc::new(v.clone()))
          .ok_or_else(|| anyhow!("failed to resolve relative schema"))
      }
      None => {
        let val = self.load_schema(&url).await?;
        drop(self.cache.store(url, val.clone()));
        Ok(val)
      }
    }
  }

  /// Compile a validator, resolving external `$ref`s at build time.
  ///
  /// `jsonschema` resolves references synchronously through the [`Retrieve`] trait, but our
  /// schema fetching is async (network / `Environment` I/O). `CacheRetriever` therefore serves
  /// only the in-memory cache and records any reference it could not satisfy; when the build
  /// fails on a missing reference we fetch it asynchronously, cache it, and rebuild. The
  /// `attempted` set guarantees progress (and termination) for genuinely unresolvable refs.
  async fn create_validator(&self, schema: &Value) -> Result<Validator, anyhow::Error> {
    let mut attempted: HashSet<Url> = HashSet::new();

    loop {
      let missing = Arc::new(Mutex::new(Vec::new()));
      let retriever = CacheRetriever {
        store:   self.cache().memory_store(),
        missing: missing.clone(),
      };

      let build_result = jsonschema::options()
        .with_retriever(retriever)
        .with_format("semver", formats::semver)
        .with_format("semver-requirement", formats::semver_req)
        .should_validate_formats(true)
        .build(schema);

      match build_result {
        Ok(validator) => return Ok(validator),
        Err(err) => {
          let requested = std::mem::take(&mut *missing.lock());
          let fresh: Vec<Url> = requested.into_iter().filter(|url| attempted.insert(url.clone())).collect();

          if fresh.is_empty() {
            return Err(anyhow!("invalid schema: {err}"));
          }

          for url in fresh {
            self
              .load_schema(&url)
              .await
              .with_context(|| format!("failed to load referenced schema {url}"))?;
          }
        }
      }
    }
  }

  async fn fetch_external(&self, schema_url: &Url) -> Result<Value, anyhow::Error> {
    let _permit = self.concurrent_requests.acquire().await?;
    match schema_url.scheme() {
      "http" | "https" => {
        let Some(http) = &self.http else {
          return Err(anyhow!("HTTP schema transport is unavailable for schema `{schema_url}`"));
        };
        Ok(http.get(schema_url.clone()).send().await?.json().await?)
      }
      "file" => Ok(serde_json::from_slice(
        &self
          .env
          .read_file(
            self
              .env
              .to_file_path_normalized(schema_url)
              .ok_or_else(|| anyhow!("invalid file path"))?
              .as_ref(),
          )
          .await?,
      )?),
      scheme => Err(anyhow!("the scheme `{scheme}` is not supported")),
    }
  }
}

impl<E: Environment> Schemas<E> {
  #[tracing::instrument(skip_all, fields(%schema_url, %path))]
  pub async fn schemas_at_path(&self, schema_url: &Url, value: &Value, path: &Keys) -> Result<Vec<(Keys, Arc<Value>)>, anyhow::Error> {
    let mut schemas = Vec::new();
    let schema = self.load_schema(schema_url).await?;
    self
      .collect_schemas(schema_url, &schema, value, Keys::empty(), path, &mut schemas)
      .await?;

    schemas = schemas
      .into_iter()
      .unique_by(|(k, s)| (k.clone(), ArcHashValue(s.clone())))
      .collect();

    Ok(schemas)
  }

  #[tracing::instrument(skip_all, fields(%path))]
  #[async_recursion(?Send)]
  #[must_use]
  async fn collect_schemas(
    &self,
    root_url: &Url,
    schema: &Value,
    value: &Value,
    full_path: Keys,
    path: &Keys,
    schemas: &mut Vec<(Keys, Arc<Value>)>,
  ) -> Result<(), anyhow::Error> {
    if !schema.is_object() {
      return Ok(());
    }

    if let Some(r) = schema.schema_ref() {
      let url = reference_url(root_url, r).ok_or_else(|| anyhow!("could not determine schema URL"))?;
      let schema = self.resolve_schema(url).await?;
      return self
        .collect_schemas(root_url, &schema, value, full_path.clone(), path, schemas)
        .await;
    }

    let composition = composition_only_kind(schema);
    let preserve_all_of_wrapper = path.is_empty() && composition == Some(CompositionKind::Intersection);

    if !preserve_all_of_wrapper {
      if let Some(one_ofs) = schema["oneOf"].as_array() {
        for one_of in one_ofs {
          self
            .collect_schemas(root_url, one_of, value, full_path.clone(), path, schemas)
            .await?;
        }
      }

      if let Some(any_ofs) = schema["anyOf"].as_array() {
        for any_of in any_ofs {
          self
            .collect_schemas(root_url, any_of, value, full_path.clone(), path, schemas)
            .await?;
        }
      }

      if let Some(all_ofs) = schema["allOf"].as_array() {
        for all_of in all_ofs {
          self
            .collect_schemas(root_url, all_of, value, full_path.clone(), path, schemas)
            .await?;
        }
      }
    }

    let Some(key) = path.iter().next() else {
      if !matches!(
        composition,
        Some(CompositionKind::ExclusiveAlternative | CompositionKind::Alternative)
      ) {
        schemas.push((full_path.clone(), Arc::new(schema.clone())));
      }
      return Ok(());
    };

    let child_path = path.skip_left(1);

    match key {
      KeyOrIndex::Key(k) => {
        // For array of tables.
        self
          .collect_schemas(
            root_url,
            &schema["items"][k.value()],
            value,
            full_path.join(k.clone()),
            &child_path,
            schemas,
          )
          .await?;

        self
          .collect_schemas(
            root_url,
            &schema["properties"][k.value()],
            &value[k.value()],
            full_path.join(k.clone()),
            &child_path,
            schemas,
          )
          .await?;

        self
          .collect_schemas(
            root_url,
            &schema["additionalProperties"],
            &value[k.value()],
            full_path.join(k.clone()),
            &child_path,
            schemas,
          )
          .await?;

        if let Some(pattern_props) = schema["patternProperties"].as_object() {
          for (pattern, pattern_schema) in pattern_props {
            let regex = Regex::new(pattern).with_context(|| {
              format!(
                "invalid patternProperties regex `{pattern}` while resolving key `{}` at schema `{root_url}` path `{full_path}`",
                k.value()
              )
            })?;
            if regex.is_match(k.value()) {
              self
                .collect_schemas(
                  root_url,
                  pattern_schema,
                  &value[k.value()],
                  full_path.join(k.clone()),
                  &child_path,
                  schemas,
                )
                .await?;
            }
          }
        }
      }
      KeyOrIndex::Index(idx) => {
        if schema["items"].is_array() {
          self
            .collect_schemas(
              root_url,
              &schema["items"][idx],
              &value[idx],
              full_path.join(*idx),
              &child_path,
              schemas,
            )
            .await?;
        } else {
          self
            .collect_schemas(root_url, &schema["items"], &value[idx], full_path.join(*idx), &child_path, schemas)
            .await?;
        }
      }
    }

    Ok(())
  }

  #[tracing::instrument(skip_all, fields(%schema_url, %path))]
  pub async fn possible_schemas_from(
    &self,
    schema_url: &Url,
    value: &Value,
    path: &Keys,
    max_depth: usize,
  ) -> Result<Vec<(Keys, Keys, Arc<Value>)>, anyhow::Error> {
    let schemas = self.schemas_at_path(schema_url, value, path).await?;

    let mut children = Vec::with_capacity(schemas.len());

    for (path, schema) in schemas {
      let mut context = ChildSchemaContext {
        root_url:  schema_url,
        root_path: &path,
        schemas:   &mut children,
      };
      self
        .collect_child_schemas(&mut context, &schema, &Keys::empty(), max_depth)
        .await;
    }

    children = children
      .into_iter()
      .unique_by(|(k1, k2, s)| (k1.clone(), k2.clone(), ArcHashValue(s.clone())))
      .collect();

    Ok(children)
  }

  #[async_recursion(?Send)]
  #[must_use]
  async fn collect_child_schemas(&self, context: &mut ChildSchemaContext<'_>, schema: &Value, path: &Keys, depth: usize) {
    if !schema.is_object() || depth == 0 {
      return;
    }

    if let Some(schema) = self.ref_schema_value(context.root_url, schema).await {
      return self.collect_child_schemas(context, &schema, path, depth).await;
    }

    if let Some(composition) = composition_only_kind(schema) {
      let branches = composition.branches(schema);
      if composition == CompositionKind::Intersection {
        let mut merged_all_of = Value::Object(serde_json::Map::default());
        for branch in branches {
          merged_all_of.merge(match self.ref_schema_value(context.root_url, branch).await {
            Some(ref resolved) => resolved,
            None => branch,
          });
        }

        let mut wrapper = schema.clone();
        if let Some(object) = wrapper.as_object_mut() {
          object.remove("allOf");
        }
        merged_all_of.merge(&wrapper);

        self.collect_child_schemas(context, &merged_all_of, path, depth).await;
        return;
      }

      for branch in branches {
        self.collect_child_schemas(context, branch, path, depth).await;
      }
      return;
    }

    for composition in [
      CompositionKind::ExclusiveAlternative,
      CompositionKind::Alternative,
      CompositionKind::Intersection,
    ] {
      for branch in composition.branches(schema) {
        self.collect_child_schemas(context, branch, path, depth).await;
      }
    }

    context
      .schemas
      .push((context.root_path.extend(path.clone()), path.clone(), Arc::new(schema.clone())));

    let child_depth = depth.saturating_sub(1);

    if let Some(map) = schema["properties"].as_object() {
      for (k, v) in map {
        self
          .collect_child_schemas(context, v, &path.join(Key::from(k)), child_depth)
          .await;
      }
    }
  }

  async fn ref_schema_value(&self, root_url: &Url, schema: &Value) -> Option<Arc<Value>> {
    if let Some(r) = schema.schema_ref() {
      let url = match reference_url(root_url, r).ok_or_else(|| anyhow!("could not determine schema URL")) {
        Ok(u) => u,
        Err(error) => {
          tracing::error!(?error, "failed to resolve schema");
          return None;
        }
      };
      let schema = match self.resolve_schema(url).await {
        Ok(s) => s,
        Err(error) => {
          tracing::error!(?error, "failed to resolve schema");
          return None;
        }
      };

      Some(schema)
    } else {
      None
    }
  }
}

fn reference_url(root_url: &Url, reference: &str) -> Option<Url> {
  if !reference.starts_with('#') {
    return Url::parse(reference).ok();
  }
  let mut url = root_url.clone();
  url.set_fragment(Some(reference.trim_start_matches("#/")));
  Some(url)
}

pub trait ValueExt {
  fn is_schema_ref(&self) -> bool;
  fn schema_ref(&self) -> Option<&str>;
}

impl ValueExt for Value {
  fn is_schema_ref(&self) -> bool {
    self["$ref"].is_string()
  }

  fn schema_ref(&self) -> Option<&str> {
    self["$ref"].as_str()
  }
}

/// A synchronous [`Retrieve`] that serves only the in-memory schema cache.
///
/// `jsonschema` resolves `$ref`s synchronously while building a validator, but our schema
/// fetching is async. Any reference not already cached is recorded in `missing` and reported as
/// an error; [`Schemas::create_validator`] uses that list to fetch the schema and rebuild.
struct CacheRetriever {
  store:   Arc<Mutex<LruCache<Url, Arc<Value>>>>,
  missing: Arc<Mutex<Vec<Url>>>,
}

impl Retrieve for CacheRetriever {
  fn retrieve(&self, uri: &jsonschema::Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let url = Url::parse(uri.as_str())?;
    match self.store.lock().get(&url).cloned() {
      Some(schema) => Ok((*schema).clone()),
      None => {
        self.missing.lock().push(url);
        Err(format!("schema `{uri}` is not cached yet").into())
      }
    }
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
  Property(String),
  Index(usize),
}

impl SchemaValidationError {
  fn from_jsonschema(error: &jsonschema::ValidationError<'_>) -> Self {
    let additional_properties = match error.kind() {
      ValidationErrorKind::AdditionalProperties {
        unexpected,
      } => Some(unexpected.clone()),
      _ => None,
    };

    let instance_path = error
      .instance_path()
      .into_iter()
      .map(|segment| match segment {
        jsonschema::paths::LocationSegment::Property(p) => PathSegment::Property(p.to_string()),
        jsonschema::paths::LocationSegment::Index(i) => PathSegment::Index(i),
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
  pub keys:              Keys,
  pub node:              dom::Node,
  pub message:           String,
  additional_properties: bool,
}

impl NodeValidationError {
  fn new(root: &dom::Node, error: SchemaValidationError) -> Result<Self, anyhow::Error> {
    let mut keys = Keys::empty();
    let mut node = root.clone();

    if let Some(unexpected) = &error.additional_properties {
      keys = keys.extend(unexpected.iter().map(Key::from).map(KeyOrIndex::Key));
    }

    'outer: for segment in &error.instance_path {
      match segment {
        PathSegment::Property(p) => match node {
          dom::Node::Table(t) => {
            let entries = t.entries().read();
            for (k, entry) in entries.iter() {
              if k.value() == p.as_str() {
                keys = keys.join(k.clone());
                node = entry.clone();
                continue 'outer;
              }
            }
            return Err(anyhow!("invalid key"));
          }
          _ => return Err(anyhow!("invalid key")),
        },
        PathSegment::Index(idx) => {
          node = node.try_get(*idx).map_err(|_| anyhow!("invalid index"))?;
          keys = keys.join(*idx);
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

  #[must_use]
  pub fn text_ranges(&self) -> Box<dyn Iterator<Item = TextRange> + '_> {
    if self.additional_properties {
      let include_children = false;

      if self.keys.is_empty() {
        return Box::new(self.node.text_ranges(include_children));
      }

      Box::new(
        self
          .keys
          .clone()
          .into_iter()
          .flat_map(move |key| self.node.get(key).text_ranges(include_children)),
      )
    } else {
      Box::new(self.node.text_ranges(true))
    }
  }
}

mod formats {
  pub(super) fn semver(value: &str) -> bool {
    semver::Version::parse(value).is_ok()
  }

  pub(super) fn semver_req(value: &str) -> bool {
    semver::VersionReq::parse(value).is_ok()
  }
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::sync::Arc;

  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::dom::Keys;
  use taplo::parser::parse;
  use url::Url;

  use super::NodeValidationError;
  use super::PathSegment;
  use super::SchemaValidationError;
  use super::Schemas;
  use super::builtins;
  use crate::test_support::TestEnvironment;
  use crate::test_support::ensure_anyhow;

  fn url(value: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(value), "the schema fixture URL must parse")
  }

  fn keys(value: &str) -> Result<Keys, TestFailure> {
    ensure_ok(value.parse(), "the schema fixture path must parse")
  }

  async fn seed(schemas: &Schemas<TestEnvironment>, schema_url: &Url, schema: Value) {
    schemas.add_schema(schema_url, Arc::new(schema)).await;
  }

  #[test]
  fn offline_transport_preserves_local_schema_capabilities() -> Result<(), TestFailure> {
    futures::executor::block_on(async {
      let environment = TestEnvironment::default();
      environment.insert_file(
        "/workspace/file-schema.json",
        serde_json::to_vec(&json!({ "title": "file" })).unwrap_or_default(),
      );
      let schemas = Schemas::new_offline(environment.clone());

      let builtin_url = url(builtins::TAPLO_CONFIG_URL)?;
      ensure_anyhow(schemas.load_schema(&builtin_url).await, "offline mode must retain built-in schemas")?;

      let memory_url = url("https://example.com/in-memory.json")?;
      seed(&schemas, &memory_url, json!({ "title": "memory" })).await;
      let memory = ensure_anyhow(
        schemas.load_schema(&memory_url).await,
        "offline mode must retain explicitly seeded memory schemas",
      )?;
      ensure(memory["title"].as_str() == Some("memory"), "memory schema contents")?;

      let file_url = url("file:///workspace/file-schema.json")?;
      let file = ensure_anyhow(schemas.load_schema(&file_url).await, "offline mode must retain file-backed schemas")?;
      ensure(file["title"].as_str() == Some("file"), "file schema contents")?;

      let disk_url = url("https://example.com/disk.json")?;
      schemas.cache().set_cache_path(Some(PathBuf::from("/cache")));
      ensure_anyhow(
        schemas
          .cache()
          .save(disk_url.clone(), Arc::new(json!({ "title": "disk" })))
          .await,
        "the disk schema fixture must persist",
      )?;
      let disk_reader = Schemas::new_offline(environment);
      disk_reader.cache().set_cache_path(Some(PathBuf::from("/cache")));
      let disk = ensure_anyhow(
        disk_reader.load_schema(&disk_url).await,
        "offline mode must retain disk-cached schemas",
      )?;
      ensure(disk["title"].as_str() == Some("disk"), "disk schema contents")?;

      let remote_url = url("https://example.com/not-cached.json")?;
      let remote_error = ensure_some(
        disk_reader.load_schema(&remote_url).await.err(),
        "offline remote schema loading must fail",
      )?;
      ensure_contains(
        &remote_error.to_string(),
        "HTTP schema transport is unavailable",
        "offline schema errors must explain the missing transport",
      )?;
      let catalog_error = ensure_some(
        disk_reader.associations().add_from_catalog(&remote_url).await.err(),
        "offline remote catalog loading must fail",
      )?;
      ensure_contains(
        &catalog_error.to_string(),
        "HTTP schema transport is unavailable",
        "offline catalog errors must explain the missing transport",
      )
    })
  }

  #[test]
  fn pattern_properties_match_reject_and_report_invalid_regex() -> Result<(), TestFailure> {
    futures::executor::block_on(async {
      let schemas = Schemas::new_offline(TestEnvironment::default());
      let matching_url = url("https://example.com/pattern.json")?;
      seed(
        &schemas,
        &matching_url,
        json!({
            "patternProperties": {
                "^foo$": { "title": "matched" }
            }
        }),
      )
      .await;
      let value = json!({ "foo": 1, "bar": 2 });
      let matching = ensure_anyhow(
        schemas.schemas_at_path(&matching_url, &value, &keys("foo")?).await,
        "a valid matching pattern must resolve",
      )?;
      ensure(
        matching.iter().any(|(_, schema)| schema["title"] == "matched"),
        "a matching pattern property must contribute its schema",
      )?;
      let nonmatching = ensure_anyhow(
        schemas.schemas_at_path(&matching_url, &value, &keys("bar")?).await,
        "a valid nonmatching pattern must be skipped normally",
      )?;
      ensure(nonmatching.is_empty(), "a valid nonmatching pattern must not contribute a schema")?;

      let invalid_url = url("https://example.com/invalid-pattern.json")?;
      seed(
        &schemas,
        &invalid_url,
        json!({ "patternProperties": { "[": { "title": "invalid" } } }),
      )
      .await;
      let invalid = ensure_some(
        schemas.schemas_at_path(&invalid_url, &value, &keys("foo")?).await.err(),
        "an invalid pattern must return a resolution error",
      )?;
      ensure_contains(
        &invalid.to_string(),
        "invalid patternProperties regex `[`",
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
    futures::executor::block_on(async {
      let schemas = Schemas::new_offline(TestEnvironment::default());
      let wrapper_url = url("https://example.com/wrapper.json")?;
      seed(
        &schemas,
        &wrapper_url,
        json!({
            "description": "wrapper docs",
            "allOf": [
                { "properties": { "left": { "type": "string" } } },
                { "properties": { "right": { "type": "integer" } } }
            ]
        }),
      )
      .await;
      let wrapper = ensure_anyhow(
        schemas.possible_schemas_from(&wrapper_url, &json!({}), &Keys::empty(), 2).await,
        "a composition-only allOf wrapper must traverse",
      )?;
      ensure(
        wrapper
          .iter()
          .any(|(_, relative, schema)| relative.is_empty() && schema["description"] == "wrapper docs"),
        "wrapper metadata must override and survive the merged composition",
      )?;
      ensure(
        wrapper.iter().any(|(_, relative, _)| relative.dotted() == "left")
          && wrapper.iter().any(|(_, relative, _)| relative.dotted() == "right"),
        "all allOf branch properties must survive the wrapper merge",
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
      )
      .await;
      let regular = ensure_anyhow(
        schemas.possible_schemas_from(&regular_url, &json!({}), &Keys::empty(), 2).await,
        "a regular schema with allOf must traverse independently",
      )?;
      ensure(
        regular.iter().any(|(_, relative, _)| relative.dotted() == "own")
          && regular.iter().any(|(_, relative, _)| relative.dotted() == "branch"),
        "regular own properties and allOf branch properties must both be exposed",
      )?;

      let empty_url = url("https://example.com/empty-composition.json")?;
      seed(&schemas, &empty_url, json!({ "title": "empty", "allOf": [] })).await;
      let zero_depth = ensure_anyhow(
        schemas.possible_schemas_from(&empty_url, &json!({}), &Keys::empty(), 0).await,
        "zero-depth traversal must terminate normally",
      )?;
      ensure(zero_depth.is_empty(), "zero depth must return no child schemas")?;
      let positive_depth = ensure_anyhow(
        schemas.possible_schemas_from(&empty_url, &json!({}), &Keys::empty(), 1).await,
        "an empty composition array must not suppress the schema",
      )?;
      ensure(
        positive_depth
          .iter()
          .any(|(_, relative, schema)| relative.is_empty() && schema["title"] == "empty"),
        "empty allOf must retain the schema itself at positive depth",
      )
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

    let parsed = parse("known = \"wrong\"\nunexpected = 1\n");
    ensure(parsed.errors.is_empty(), "the validation range fixture must parse cleanly")?;
    let root = parsed.into_dom();
    let additional_node = ensure_anyhow(
      NodeValidationError::new(&root, additional.clone()),
      "the additional-properties error must resolve to the DOM",
    )?;
    let additional_ranges: Vec<_> = additional_node.text_ranges().collect();
    ensure(
      !additional_ranges.is_empty(),
      "an unexpected property must retain its concrete DOM range",
    )?;
    let typed_node = ensure_anyhow(
      NodeValidationError::new(&root, typed.clone()),
      "the property-type error must resolve to the DOM child",
    )?;
    ensure(
      typed_node.text_ranges().next().is_some(),
      "a normal child validation error must retain a child range",
    )
  }
}
