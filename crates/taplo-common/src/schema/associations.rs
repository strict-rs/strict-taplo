//! Rules that decide which JSON Schema applies to a TOML document.
//!
//! An association pairs a matching rule — a glob, a regular expression, or one exact document
//! URL — with a schema URL and a priority. Several rules may match the same document; the
//! highest priority wins. Priorities are ordered so that the more specific and more local a
//! source is, the later it is allowed to decide: a built-in loses to a catalog, a catalog loses
//! to configuration, and an in-document `$schema` key or `#:schema` directive beats everything.
//!
//! Each association also records the source that contributed it, which lets one source's
//! associations be replaced or removed without disturbing the others.

use std::borrow::Cow;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::iter::empty;
use std::path::Path;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use futures::future::LocalBoxFuture;
use parking_lot::RwLock;
use parking_lot::RwLockReadGuard;
use regex::Regex;
use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Error;
use serde_json::Value;
use serde_json::json;
use taplo::dom::Node;
use thiserror::Error;
use url::ParseError;
use url::Url;

use super::builtins;
use super::cache::Cache;
use super::cache::CacheError;
use super::transport::ConcurrentTransport;
use super::transport::SchemaTransport;
use super::transport::TransportError;
use crate::IndexMap;
use crate::config::Config;
use crate::config::SchemaOptions;
use crate::util::GlobRule;
use crate::util::GlobRuleError;
use crate::util::normalize_str;

/// Default remote schema catalogs.
pub const DEFAULT_CATALOGS: &[&str] = &["https://json.schemastore.org/api/json/catalog.json"];

/// A typed failure while building or refreshing schema associations.
#[derive(Debug, Error)]
pub enum AssociationError {
  /// Schema transport failed.
  #[error(transparent)]
  Transport(#[from] TransportError),
  /// Schema cache access failed.
  #[error(transparent)]
  Cache(#[from] CacheError),
  /// A catalog document could not be decoded.
  #[error("schema catalog `{url}` is invalid")]
  Catalog {
    /// Catalog URL.
    url:    Url,
    /// Underlying catalog decoder failure.
    #[source]
    source: serde_json::Error,
  },
  /// A catalog could not be serialized for persistence.
  #[error("schema catalog `{url}` could not be serialized")]
  CatalogSerialization {
    /// Catalog URL.
    url:    Url,
    /// Underlying JSON encoder failure.
    #[source]
    source: serde_json::Error,
  },
  /// An association glob was invalid.
  #[error("invalid association glob for `{name}`")]
  Glob {
    /// Schema/catalog entry name.
    name:   String,
    /// Underlying glob failure.
    #[source]
    source: GlobRuleError,
  },
  /// An association regular expression was invalid.
  #[error("invalid association regular expression `{pattern}` for `{name}`")]
  Regex {
    /// Rejected expression.
    pattern: String,
    /// Schema/catalog entry name.
    name:    String,
    /// Underlying regex failure.
    #[source]
    source:  Box<regex::Error>,
  },
  /// A schema directive or field contained an invalid URL/path.
  #[error("invalid schema location `{input}` in {source_name}")]
  InvalidSchemaLocation {
    /// Rejected source text.
    input:       String,
    /// Stable source description.
    source_name: &'static str,
    /// Underlying URL parser failure.
    #[source]
    source:      ParseError,
  },
  /// A schema directive contained no location.
  #[error("schema directive is empty")]
  EmptyDirective,
}

/// One matching rule and the schema association it selects.
type AssociationEntry = (AssociationRule, SchemaAssociation);

/// Shared ordered association state.
type SharedAssociations = Arc<RwLock<Vec<AssociationEntry>>>;

/// Generate one execution-model-specific catalog operation family.
macro_rules! catalog_operations {
  (
    ($add_from_catalog:ident, $replace_catalogs:ident),
    $load_catalog:ident,
    ($cache_load:ident, $cache_save:ident);
    $future:ident,
    $box_with:ident,
    { $($bounds:tt)* }
  ) => {
    /// Replace associations owned by one catalog.
    ///
    /// The catalog is validated completely before the live association set is
    /// changed.
    ///
    /// # Errors
    ///
    /// Returns [`AssociationError`] for transport, cache, serialization, glob, or
    /// regular-expression failures.
    pub fn $add_from_catalog<'associations>(
      &'associations self,
      url: &'associations Url,
    ) -> $future<'associations, Result<(), AssociationError>>
    $($bounds)*
    {
      async move {
        let index = self.$load_catalog(url).await?;
        let replacements = catalog_replacements(url, &index)?;
        self.replace_catalog(url, replacements);
        Ok(())
      }
      .$box_with()
    }

    /// Replace the complete catalog-owned association set atomically.
    ///
    /// Every catalog is loaded and validated before the live association set is changed.
    ///
    /// # Errors
    ///
    /// Returns [`AssociationError`] for transport, cache, serialization, glob, or
    /// regular-expression failures in any catalog.
    pub fn $replace_catalogs<'associations>(
      &'associations self,
      urls: &'associations [Url],
    ) -> $future<'associations, Result<(), AssociationError>>
    $($bounds)*
    {
      async move {
        let mut replacements = Vec::new();
        for url in urls {
          let catalog = self.$load_catalog(url).await?;
          replacements.extend(catalog_replacements(url, &catalog)?);
        }
        self.replace_source(source::CATALOG, replacements);
        Ok(())
      }
      .$box_with()
    }

    /// Load, transform, and optionally persist one catalog.
    fn $load_catalog<'associations>(
      &'associations self,
      index_url: &'associations Url,
    ) -> $future<'associations, Result<SchemaCatalog, AssociationError>>
    $($bounds)*
    {
      async move {
        if let Ok(cached_catalog) = self.cache.$cache_load(index_url, false).await {
          return serde_json::from_value(Arc::unwrap_or_clone(cached_catalog)).map_err(|source| AssociationError::Catalog {
            url: index_url.clone(),
            source,
          });
        }

        let catalog_document = match self.transport.read_json(index_url.clone()).await {
          Ok(retrieved_catalog) => retrieved_catalog,
          Err(error) => {
            if let Ok(stale_catalog) = self.cache.$cache_load(index_url, true).await {
              return serde_json::from_value(Arc::unwrap_or_clone(stale_catalog)).map_err(|source| AssociationError::Catalog {
                url: index_url.clone(),
                source,
              });
            }
            return Err(AssociationError::Transport(error));
          }
        };
        let mut index = serde_json::from_value::<SchemaCatalog>(catalog_document).map_err(|source| AssociationError::Catalog {
          url: index_url.clone(),
          source,
        })?;

        index.transform_paths();
        let serialized = serde_json::to_value(&index).map_err(|source| AssociationError::CatalogSerialization {
          url: index_url.clone(),
          source,
        })?;
        self
          .cache
          .$cache_save(index_url.clone(), Arc::new(serialized))
          .await?;

        Ok(index)
      }
      .$box_with()
    }
  };
}

/// Priorities that order competing associations; the highest matching one wins.
///
/// The values are spaced so a caller can slot a custom source between two of them.
pub mod priority {
  /// Schemas compiled into Taplo itself.
  pub const BUILTIN: usize = 10;
  /// Schemas offered by a remote catalog.
  pub const CATALOG: usize = 25;
  /// The `schema` option in a configuration file's global options.
  pub const CONFIG: usize = 50;
  /// The `schema` option of a configuration `[[rule]]`, which is narrower than the global one.
  pub const CONFIG_RULE: usize = 51;
  /// Schemas configured through the editor's language-server settings.
  pub const LSP_CONFIG: usize = 60;
  /// A `$schema` key in the document root.
  pub const SCHEMA_FIELD: usize = 70;
  /// A `#:schema` directive in the document's header comments.
  pub const DIRECTIVE: usize = 75;
  /// Reserved for an association that must not be overridden.
  pub const MAX: usize = usize::MAX;
}

/// Labels identifying which source contributed an association.
///
/// The label is stored under the `source` key of [`SchemaAssociation::meta`] and identifies the
/// entries one source owns, so they can be replaced or dropped as a set.
pub mod source {
  /// Compiled into Taplo itself.
  pub const BUILTIN: &str = "builtin";
  /// Loaded from a remote catalog.
  pub const CATALOG: &str = "catalog";
  /// Declared in a configuration file.
  pub const CONFIG: &str = "config";
  /// Declared in the editor's language-server settings.
  pub const LSP_CONFIG: &str = "lsp_config";
  /// Added directly by an embedding application.
  pub const MANUAL: &str = "manual";
  /// Read from a `$schema` key in the document root.
  pub const SCHEMA_FIELD: &str = "$schema";
  /// Read from a `#:schema` directive in the document's header comments.
  pub const DIRECTIVE: &str = "directive";
}

/// The live set of document-to-schema associations.
///
/// Cloning shares the same rules, so every holder observes the same association set. Rules are
/// grouped by the [`source`] label that contributed them, and each mutating operation replaces
/// one source's entries as a unit: a catalog refresh or a reopened document never disturbs
/// associations another source owns. Catalog loads are validated completely before the live set
/// changes, so a failing refresh leaves the previous associations in place.
#[derive(Clone)]
pub struct SchemaAssociations<T: SchemaTransport> {
  /// Transport shared with the schema cache.
  transport:    T,
  /// Ordered association rules.
  associations: SharedAssociations,
  /// Schema/catalog cache.
  cache:        Cache<T>,
}

impl<T: SchemaTransport> Debug for SchemaAssociations<T> {
  /// Render how many rules are installed rather than the rules themselves.
  ///
  /// A set another thread is currently writing is reported as `None` instead of being waited
  /// for, so formatting never blocks.
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    let rules = self.associations.try_read().map(|associations| associations.len());
    f.debug_struct("SchemaAssociations")
      .field("rules", &rules)
      .finish_non_exhaustive()
  }
}

impl<T: SchemaTransport> SchemaAssociations<T> {
  /// Construct associations and install the built-in Taplo configuration rule.
  ///
  /// # Errors
  ///
  /// Returns [`AssociationError`] if a compiled-in URL or expression is invalid.
  #[allow(
    clippy::single_call_fn,
    reason = "association construction owns installation of the built-in configuration-schema rule"
  )]
  pub(crate) fn new(transport: T, cache: Cache<T>) -> Result<Self, AssociationError> {
    let this = Self {
      cache,
      transport,
      associations: Arc::default(),
    };
    this.add_builtins()?;
    Ok(this)
  }

  /// Add one association without touching the existing ones.
  ///
  /// Use [`SchemaAssociations::replace_source`] instead when the caller owns a whole source and
  /// needs its previous entries to disappear.
  pub fn add(&self, rule: AssociationRule, assoc: SchemaAssociation) {
    self.associations.write().push((rule, assoc));
  }

  /// Keep only the associations for which `f` returns `true`.
  pub fn retain(&self, f: impl Fn(&(AssociationRule, SchemaAssociation)) -> bool) {
    self.associations.write().retain(f);
  }

  /// Borrow the current association set.
  ///
  /// The returned guard blocks every mutating operation for as long as it is held.
  pub fn read(&self) -> RwLockReadGuard<'_, Vec<(AssociationRule, SchemaAssociation)>> {
    self.associations.read()
  }

  /// Clear all associations.
  ///
  /// Note that this will completely remove all associations,
  /// even built-in ones that will have to be added again.
  pub fn clear(&self) {
    self.associations.write().clear();
  }

  /// Replace built-in associations with the current built-in set.
  ///
  /// # Errors
  ///
  /// Returns [`AssociationError`] if a compiled-in URL or expression is invalid.
  pub fn add_builtins(&self) -> Result<(), AssociationError> {
    let regex = Regex::new(r".*\.?taplo\.toml$").map_err(|source| AssociationError::Regex {
      pattern: r".*\.?taplo\.toml$".into(),
      name:    "Taplo".into(),
      source:  Box::new(source),
    })?;
    let url = builtins::TAPLO_CONFIG_URL
      .parse()
      .map_err(|source| AssociationError::InvalidSchemaLocation {
        input: builtins::TAPLO_CONFIG_URL.into(),
        source_name: "built-in association",
        source,
      })?;
    self.replace_source(
      source::BUILTIN,
      Vec::from([(AssociationRule::Regex(regex), SchemaAssociation {
        url,
        meta: json!({
            "name": "Taplo",
            "description": "Taplo configuration file.",
            "source": source::BUILTIN
        }),
        priority: priority::BUILTIN,
      })]),
    );
    Ok(())
  }

  schema_execution_families!(
    catalog_operations;
    (
      (add_from_catalog, replace_catalogs),
      (add_from_catalog_concurrent, replace_catalogs_concurrent)
    ),
    (load_catalog, load_catalog_concurrent),
    (
      (load, save_if_configured),
      (load_concurrent, save_if_configured_concurrent)
    ),
  );

  /// Add the schema from either a directive or a `$schema` key in the root.
  ///
  /// # Errors
  ///
  /// Returns [`AssociationError`] when a declared schema location is empty,
  /// malformed, or cannot be resolved through the host path model.
  pub fn add_from_document(&self, doc_url: &Url, root: &Node) -> Result<(), AssociationError> {
    let mut replacements = Vec::new();

    if let Some(directive_value) = schema_directive_value(root) {
      let schema_url = directive_schema_url(&self.transport, doc_url, &directive_value)?;
      replacements.push((AssociationRule::Url(doc_url.clone()), SchemaAssociation {
        url:      schema_url,
        priority: priority::DIRECTIVE,
        meta:     json!({ "source": source::DIRECTIVE }),
      }));
    }

    if let Some(Node::Str(schema_value)) = root.get_key("$schema") {
      let schema_url = schema_field_url(doc_url, schema_value.value())?;
      replacements.push((AssociationRule::Url(doc_url.clone()), SchemaAssociation {
        url:      schema_url,
        priority: priority::SCHEMA_FIELD,
        meta:     json!({ "source": source::SCHEMA_FIELD }),
      }));
    }
    self.replace_document_sources(doc_url, replacements);
    Ok(())
  }

  /// Remove only directive and `$schema` associations owned by one document.
  pub fn remove_from_document(&self, doc_url: &Url) {
    self.replace_document_sources(doc_url, Vec::new());
  }

  /// Replace the configuration-owned associations with those of one prepared configuration.
  ///
  /// Every `[[rule]]` that targets whole documents and names an enabled schema contributes an
  /// association, as do the global options. Key-scoped rules are ignored, because a schema
  /// applies to a document rather than to part of one. The configuration must already be
  /// prepared: rules without a compiled file matcher are skipped.
  pub fn add_from_config(&self, config: &Config) {
    let mut replacements = Vec::new();

    for rule in &config.rule {
      if rule.keys.is_some() {
        continue;
      }
      let Some(file_rule) = rule.matcher.clone() else {
        continue;
      };

      if let Some(association) = rule
        .options
        .schema
        .as_ref()
        .and_then(|options| config_association(options, priority::CONFIG_RULE))
      {
        replacements.push((file_rule.into(), association));
      }
    }

    if let Some((file_rule, association)) = config.file_rule.clone().zip(
      config
        .global_options
        .schema
        .as_ref()
        .and_then(|options| config_association(options, priority::CONFIG)),
    ) {
      replacements.push((file_rule.into(), association));
    }
    self.replace_source(source::CONFIG, replacements);
  }

  /// Atomically replace every association owned by one source label.
  pub fn replace_source(&self, source_name: &str, replacements: Vec<(AssociationRule, SchemaAssociation)>) {
    self.replace_owned(
      |_, association| association_metadata(association, "source") == Some(source_name),
      replacements,
    );
  }

  /// Replace every rule owned by one catalog URL.
  fn replace_catalog(&self, catalog_url: &Url, replacements: Vec<(AssociationRule, SchemaAssociation)>) {
    self.replace_owned(
      |_, association| {
        association_metadata(association, "source") == Some(source::CATALOG)
          && association_metadata(association, "catalog_url") == Some(catalog_url.as_str())
      },
      replacements,
    );
  }

  /// Replace directive and schema-field rules owned by one document.
  fn replace_document_sources(&self, document_url: &Url, replacements: Vec<(AssociationRule, SchemaAssociation)>) {
    self.replace_owned(
      |rule, association| {
        matches!(rule, AssociationRule::Url(url) if url == document_url)
          && matches!(
            association_metadata(association, "source"),
            Some(source::DIRECTIVE | source::SCHEMA_FIELD)
          )
      },
      replacements,
    );
  }

  /// Commit one fully prepared source-owned replacement under one write lock.
  fn replace_owned(
    &self,
    mut is_owned: impl FnMut(&AssociationRule, &SchemaAssociation) -> bool,
    replacements: Vec<(AssociationRule, SchemaAssociation)>,
  ) {
    let mut associations = self.associations.write();
    associations.retain(|association| !is_owned(&association.0, &association.1));
    associations.extend(replacements);
  }

  /// Return the association that wins for one document, if any rule matches it.
  ///
  /// When several rules match, the highest [`priority`] wins; among equal priorities the last
  /// matching rule wins, so a later addition supersedes an earlier one from the same source.
  pub fn association_for(&self, file: &Url) -> Option<SchemaAssociation> {
    let association = self
      .associations
      .read()
      .iter()
      .filter(|association| association.0.is_match(file))
      .map(|association| association.1.clone())
      .max_by_key(|assoc| assoc.priority);
    if let Some(schema_association) = association.as_ref() {
      tracing::debug!(
          schema.url = %schema_association.url,
          schema.name = association_metadata(schema_association, "name").unwrap_or(""),
          schema.source = association_metadata(schema_association, "source").unwrap_or(""),
          "found schema association"
      );
    }
    association
  }
}

/// Read one textual metadata property from an association.
fn association_metadata<'a>(association: &'a SchemaAssociation, name: &str) -> Option<&'a str> {
  association.meta.get(name).and_then(Value::as_str)
}

/// Return the first document-header schema directive value.
#[allow(
  clippy::single_call_fn,
  reason = "directive discovery names the header-only precedence boundary separately from URL resolution and association replacement"
)]
fn schema_directive_value(root: &Node) -> Option<String> {
  root
    .header_comments()
    .find(|comment| comment.directive() == Some("schema"))
    .map(|comment| comment.value().to_owned())
}

/// Resolve one schema directive against its owning document and host path model.
#[allow(
  clippy::single_call_fn,
  reason = "directive URL resolution preserves the distinct absolute-host-path and document-relative semantics outside association \
            mutation"
)]
fn directive_schema_url(transport: &impl SchemaTransport, document_url: &Url, directive_value: &str) -> Result<Url, AssociationError> {
  if directive_value.is_empty() {
    return Err(AssociationError::EmptyDirective);
  }
  if let Ok(url) = directive_value.parse() {
    return Ok(url);
  }
  let resolved = if transport.is_absolute(Path::new(directive_value))? {
    format!("file://{directive_value}")
  } else {
    return document_url
      .join(directive_value)
      .map_err(|source| invalid_schema_location(directive_value, "schema directive", source));
  };
  resolved
    .parse()
    .map_err(|source| invalid_schema_location(directive_value, "schema directive", source))
}

/// Resolve one root `$schema` field against its owning document.
#[allow(
  clippy::single_call_fn,
  reason = "schema-field URL resolution keeps its dot-relative contract distinct from directive host-path handling"
)]
fn schema_field_url(document_url: &Url, schema_value: &str) -> Result<Url, AssociationError> {
  if schema_value.starts_with('.') {
    document_url
      .join(schema_value)
      .map_err(|source| invalid_schema_location(schema_value, "`$schema` field", source))
  } else {
    schema_value
      .parse()
      .map_err(|source| invalid_schema_location(schema_value, "`$schema` field", source))
  }
}

/// Attach stable source context to one rejected schema location.
fn invalid_schema_location(input: &str, source_name: &'static str, source: ParseError) -> AssociationError {
  AssociationError::InvalidSchemaLocation {
    input: input.into(),
    source_name,
    source,
  }
}

/// Compile every rule supplied by one already-loaded schema catalog.
fn catalog_replacements(catalog_url: &Url, catalog: &SchemaCatalog) -> Result<Vec<(AssociationRule, SchemaAssociation)>, AssociationError> {
  let mut replacements = Vec::new();
  match *catalog {
    SchemaCatalog::SchemaStore(ref index) => {
      for schema in &index.schemas {
        let rule = GlobRule::new(&schema.file_match, empty::<&str>()).map_err(|source| AssociationError::Glob {
          name: schema.name.clone(),
          source,
        })?;
        replacements.push((
          rule.into(),
          catalog_association(&schema.name, &schema.description, schema.url.clone(), catalog_url),
        ));
      }
    }
    SchemaCatalog::Taplo(ref index) => {
      for schema in &index.schemas {
        for pattern in &schema.extra.patterns {
          let regex = Regex::new(pattern).map_err(|source| AssociationError::Regex {
            pattern: pattern.clone(),
            name:    schema.title.clone(),
            source:  Box::new(source),
          })?;
          replacements.push((
            regex.into(),
            catalog_association(&schema.title, &schema.description, schema.url.clone(), catalog_url),
          ));
        }
      }
    }
  }
  Ok(replacements)
}

/// Build the shared association metadata for one validated catalog entry.
fn catalog_association(name: &str, description: &str, url: Url, catalog_url: &Url) -> SchemaAssociation {
  SchemaAssociation {
    url,
    meta: json!({
        "name": name,
        "description": description,
        "source": source::CATALOG,
        "catalog_url": catalog_url,
    }),
    priority: priority::CATALOG,
  }
}

/// How an association decides whether it applies to a document.
#[derive(Debug, Clone)]
pub enum AssociationRule {
  /// Match the document's path against include/exclude globs.
  ///
  /// Globs usually come from configuration files and are written as absolute paths without a
  /// scheme, so matching strips the URL scheme before comparing.
  Glob(GlobRule),
  /// Match the document's full URL against a regular expression.
  Regex(Regex),
  /// Match exactly one document URL.
  Url(Url),
}

impl AssociationRule {
  /// Compile one glob association rule.
  ///
  /// # Errors
  ///
  /// Returns [`GlobRuleError`] when the expression is invalid.
  pub fn glob(pattern: &str) -> Result<Self, GlobRuleError> {
    GlobRule::new([pattern], empty::<&str>()).map(Self::Glob)
  }

  /// Compile one regular-expression association rule.
  ///
  /// # Errors
  ///
  /// Returns [`regex::Error`] when the expression is invalid.
  pub fn regex(regex: &str) -> Result<Self, regex::Error> {
    Regex::new(regex).map(Self::Regex)
  }

  /// Return whether this rule applies to one document URL.
  #[must_use]
  pub fn is_match(&self, url: &Url) -> bool {
    match *self {
      // Glob associations typically come from config files
      // with a glob pattern that is an absolute file path
      // without a scheme.
      //
      // So in order to be a match, we need to
      // strip the scheme from the URL.
      Self::Glob(ref glob_rule) => glob_rule.is_match(&*normalize_str(
        url
          .as_str()
          .strip_prefix(url.scheme())
          .and_then(|without_scheme| without_scheme.strip_prefix("://"))
          .unwrap_or_else(|| url.path()),
      )),
      Self::Regex(ref regex) => regex.is_match(&normalize_str(url.as_str())),
      Self::Url(ref expected_url) => expected_url == url,
    }
  }
}

impl From<Regex> for AssociationRule {
  fn from(regex: Regex) -> Self {
    Self::Regex(regex)
  }
}

impl From<GlobRule> for AssociationRule {
  fn from(glob_rule: GlobRule) -> Self {
    Self::Glob(glob_rule)
  }
}

/// Convert prepared configuration options into an enabled, URL-bearing association.
fn config_association(options: &SchemaOptions, priority: usize) -> Option<SchemaAssociation> {
  if options.enabled == Some(false) {
    return None;
  }

  Some(SchemaAssociation {
    url: options.url.clone()?,
    meta: json!({ "source": source::CONFIG }),
    priority,
  })
}

/// A schema catalog in one of the two supported document shapes.
///
/// The shape is recognized from the document itself: a `SchemaStore` catalog is identified by its
/// `$schema` field, so anything else is read as a Taplo catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SchemaCatalog {
  /// A catalog following the `SchemaStore` format, matching documents by glob.
  SchemaStore(SchemaStoreCatalog),
  /// A catalog following Taplo's own format, matching documents by regular expression.
  Taplo(TaploSchemaCatalog),
}

impl SchemaCatalog {
  /// Normalize schema-store patterns so they match at every directory depth.
  fn transform_paths(&mut self) {
    if let Self::SchemaStore(ref mut index) = *self {
      for file_match in index
        .schemas
        .iter_mut()
        .flat_map(|schema| &mut schema.file_match)
        .filter(|file_match| !file_match.starts_with("**/"))
      {
        let mut normalized = String::from("**/");
        normalized.push_str(file_match);
        *file_match = normalized;
      }
    }
  }
}

/// A catalog in Taplo's own format.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TaploSchemaCatalog {
  /// Every schema the catalog offers.
  pub schemas: Vec<TaploSchemaMeta>,
}

/// Define one catalog schema-entry DTO while preserving its format-specific public fields.
macro_rules! define_catalog_schema_meta {
  (
    $(#[$metadata:meta])*
    $name:ident {
      display: $display:ident,
      display_doc: $display_doc:literal,
      $($specific_fields:tt)*
    }
  ) => {
    $(#[$metadata])*
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct $name {
      #[doc = $display_doc]
      #[serde(default)]
      pub $display:     String,
      /// Longer description of what the schema covers.
      #[serde(default)]
      pub description:  String,
      /// Location the schema is fetched from.
      pub url:          Url,
      $($specific_fields)*
    }
  };
}

define_catalog_schema_meta! {
  /// One schema entry in a Taplo catalog.
  TaploSchemaMeta {
    display: title,
    display_doc: "Display name of the schema.",
    /// Catalog-provided hash of [`url`](Self::url), used by the catalog's own tooling.
    pub url_hash: String,
    /// Authorship, versioning, and the patterns that select documents.
    #[serde(flatten)]
    pub extra: TaploSchemaExtraInfo,
  }
}

/// The additional metadata a Taplo catalog entry carries inline.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaploSchemaExtraInfo {
  /// People credited with the schema.
  pub authors:  Vec<String>,
  /// Version of the schema, when it is versioned.
  pub version:  Option<Version>,
  /// Regular expressions selecting the documents this schema applies to.
  pub patterns: Vec<String>,
}

/// A catalog in the `SchemaStore` format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaStoreCatalog {
  /// Marker field identifying the document as a `SchemaStore` catalog.
  #[serde(rename = "$schema")]
  pub schema:  SchemaStoreCatalogSchema,
  /// Every schema the catalog offers.
  pub schemas: Vec<SchemaStoreSchemaMeta>,
}

define_catalog_schema_meta! {
  /// One schema entry in a `SchemaStore` catalog.
  SchemaStoreSchemaMeta {
    display: name,
    display_doc: "Display name of the schema.",
    /// Globs selecting the documents this schema applies to.
    ///
    /// Entries are rewritten to match at any depth when the catalog is loaded.
    #[serde(default)]
    pub file_match: Vec<String>,
    /// Alternative locations for specific versions of the schema, keyed by version name.
    #[serde(default)]
    pub versions: IndexMap<String, Url>,
  }
}

/// The `$schema` value every `SchemaStore` catalog must declare.
pub const SCHEMA_STORE_CATALOG_SCHEMA_URL: &str = "https://json.schemastore.org/schema-catalog.json";

/// The `SchemaStore` catalog marker, which only decodes from
/// [`SCHEMA_STORE_CATALOG_SCHEMA_URL`].
///
/// Requiring the exact value is what distinguishes a `SchemaStore` catalog from a Taplo one while
/// decoding [`SchemaCatalog`].
#[derive(Debug, Clone, Copy)]
pub struct SchemaStoreCatalogSchema;

impl<'de> Deserialize<'de> for SchemaStoreCatalogSchema {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let schema_url = Cow::<'static, str>::deserialize(deserializer)?;

    if schema_url != SCHEMA_STORE_CATALOG_SCHEMA_URL {
      return Err(Error::custom(format!("expected $schema to be {SCHEMA_STORE_CATALOG_SCHEMA_URL}")));
    }

    Ok(Self)
  }
}

impl Serialize for SchemaStoreCatalogSchema {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    SCHEMA_STORE_CATALOG_SCHEMA_URL.serialize(serializer)
  }
}

/// The schema an [`AssociationRule`] points a matching document at.
#[derive(Debug, Clone)]
pub struct SchemaAssociation {
  /// Descriptive metadata about the association.
  ///
  /// The `source` key names the contributing [`source`] and is what source-scoped replacement
  /// matches on; `name`, `description`, and `catalog_url` are present when the contributing
  /// source knows them, and are surfaced to users.
  pub meta:     Value,
  /// Location of the schema to apply.
  pub url:      Url,
  /// Rank used to resolve competing associations; see [`priority`].
  pub priority: usize,
}

#[cfg(test)]
mod tests {
  use std::future::Future;
  use std::path::Path;
  use std::path::PathBuf;
  use std::slice::from_ref;
  use std::sync::Arc;
  use std::time::Duration;

  use futures::executor::block_on;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::parser::parse;
  use taplo_test_support::ensure_result;
  use time::OffsetDateTime;
  use url::Url;

  use super::AssociationError;
  use super::AssociationRule;
  use super::SCHEMA_STORE_CATALOG_SCHEMA_URL;
  use super::SchemaAssociation;
  use super::SchemaAssociations;
  use super::SchemaCatalog;
  use super::SchemaStoreCatalogSchema;
  use super::association_metadata;
  use super::catalog_replacements;
  use super::priority;
  use super::source;
  use crate::config::Config;
  use crate::config::Options;
  use crate::config::Rule;
  use crate::config::SchemaOptions;
  use crate::schema::cache::Cache;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use crate::schema::transport::ConcurrentSchemaTransport;
  use crate::schema::transport::OfflineSchemaTransport;
  use crate::schema::transport::SchemaTransport;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use crate::schema::transport::concurrent_http_client;
  use crate::test_support::TestEnvironment;
  /// Offline association service used by behavior tests.
  type TestAssociations = SchemaAssociations<OfflineSchemaTransport<TestEnvironment>>;
  /// Concurrent association service used to exercise the native `Send` operation family.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  type ConcurrentTestAssociations = SchemaAssociations<ConcurrentSchemaTransport<TestEnvironment>>;

  fn url(input: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(input), "the association fixture URL must parse")
  }

  fn schema_options(schema_url: Option<Url>, enabled: Option<bool>) -> Options {
    Options {
      schema:     Some(SchemaOptions {
        enabled,
        path: None,
        url: schema_url,
      }),
      formatting: None,
    }
  }

  /// Construct an unprepared configuration rule for one file pattern.
  fn schema_rule(pattern: &str, keys: Option<Vec<String>>, schema_url: Option<Url>, enabled: Option<bool>) -> Rule {
    Rule {
      name: None,
      include: Some(Vec::from([pattern.into()])),
      exclude: None,
      keys,
      options: schema_options(schema_url, enabled),
      matcher: None,
    }
  }

  fn associations(environment: TestEnvironment) -> Result<TestAssociations, TestFailure> {
    let transport = OfflineSchemaTransport::new(environment);
    let cache = ensure_result(Cache::new(transport.clone()), "the association cache must initialize")?;
    ensure_result(SchemaAssociations::new(transport, cache), "the association service must initialize")
  }

  /// Construct one native concurrent association service with the real HTTP-capable transport.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  fn concurrent_associations(environment: TestEnvironment) -> Result<ConcurrentTestAssociations, TestFailure> {
    let http = ensure_result(
      concurrent_http_client(&environment, Duration::from_secs(2)),
      "the concurrent association HTTP client must construct",
    )?;
    let transport = ConcurrentSchemaTransport::new(environment, http);
    let cache = ensure_result(Cache::new(transport.clone()), "the concurrent association cache must initialize")?;
    ensure_result(
      SchemaAssociations::new(transport, cache),
      "the concurrent association service must initialize",
    )
  }

  /// Parse one source-backed DOM root for document-association behavior.
  fn document(source_text: &str) -> Result<taplo::dom::Node, TestFailure> {
    let parsed = ensure_ok(parse(source_text), "the document-association fixture tree must build")?;
    ensure(
      parsed.diagnostics().is_empty(),
      "the document-association fixture must parse cleanly",
    )?;
    Ok(parsed.into_dom())
  }

  fn source_count<T: SchemaTransport>(associations: &SchemaAssociations<T>, expected_source: &str) -> usize {
    associations
      .read()
      .iter()
      .filter(|entry| association_metadata(&entry.1, "source") == Some(expected_source))
      .count()
  }

  /// Require one association source to own exactly the expected number of entries.
  fn ensure_source_count<T: SchemaTransport>(
    associations: &SchemaAssociations<T>,
    source_name: &str,
    expected: usize,
    context: &'static str,
  ) -> Result<(), TestFailure> {
    ensure_eq(&source_count(associations, source_name), &expected, context)
  }

  /// Parse one document fixture and install its document-owned schema association.
  fn add_document_association(associations: &TestAssociations, document_url: &Url, source_text: &str) -> Result<(), TestFailure> {
    let parsed = ensure_ok(parse(source_text), "the document-association fixture tree must build")?;
    ensure(
      parsed.diagnostics().is_empty(),
      "the document-association fixture must parse cleanly",
    )?;
    ensure_result(
      associations.add_from_document(document_url, &parsed.into_dom()),
      "the document-owned association must be accepted",
    )
  }

  /// Return the owning catalog URLs of every current catalog association.
  fn catalog_sources<T: SchemaTransport>(associations: &SchemaAssociations<T>) -> Vec<String> {
    associations
      .read()
      .iter()
      .filter(|entry| association_metadata(&entry.1, "source") == Some(source::CATALOG))
      .filter_map(|entry| association_metadata(&entry.1, "catalog_url").map(ToOwned::to_owned))
      .collect()
  }

  /// Require catalog ownership to match the supplied URLs in deterministic order.
  fn ensure_catalog_sources<T: SchemaTransport, const N: usize>(
    associations: &SchemaAssociations<T>,
    expected: [&Url; N],
    context: &'static str,
  ) -> Result<(), TestFailure> {
    let expected_sources = expected
      .into_iter()
      .map(|catalog_url| catalog_url.to_string())
      .collect::<Vec<_>>();
    ensure(catalog_sources(associations) == expected_sources, context)
  }

  /// Require one catalog operation family to replace and then clear ownership.
  async fn ensure_catalog_replacement_and_clear<T, Replacement, Clearing>(
    associations: &SchemaAssociations<T>,
    expected: &Url,
    replacement: Replacement,
    clearing: Clearing,
    contexts: [&'static str; 4],
  ) -> Result<(), TestFailure>
  where
    T: SchemaTransport,
    Replacement: Future<Output = Result<(), AssociationError>>,
    Clearing: Future<Output = Result<(), AssociationError>>,
  {
    let [replacement_context, expected_context, clearing_context, empty_context] = contexts;
    ensure_result(replacement.await, replacement_context)?;
    ensure_catalog_sources(associations, [expected], expected_context)?;
    ensure_result(clearing.await, clearing_context)?;
    ensure_catalog_sources(associations, [], empty_context)
  }

  /// Construct one Taplo catalog containing a single pattern association.
  fn catalog(schema_url: &Url, title: &str, pattern: &str) -> serde_json::Value {
    json!({
      "schemas": [{
        "title": title,
        "description": "",
        "url": schema_url,
        "urlHash": "",
        "authors": [],
        "version": null,
        "patterns": [pattern]
      }]
    })
  }

  #[test]
  fn association_rules_match_each_family_and_reject_invalid_patterns() -> Result<(), TestFailure> {
    let toml = url("file:///workspace/nested/example.toml")?;
    let json = url("file:///workspace/nested/example.json")?;
    let other = url("file:///workspace/nested/other.toml")?;

    let glob = ensure_result(AssociationRule::glob("**/*.toml"), "a valid association glob must compile")?;
    ensure(
      [glob.is_match(&toml), glob.is_match(&other), glob.is_match(&json)] == [true, true, false],
      "glob rules must match normalized document paths and reject different extensions",
    )?;
    ensure(
      AssociationRule::glob("[").is_err(),
      "an invalid association glob must retain its typed compilation failure",
    )?;

    let regex = ensure_result(
      AssociationRule::regex(r"example\.toml$"),
      "a valid association regular expression must compile",
    )?;
    ensure(
      [regex.is_match(&toml), regex.is_match(&other), regex.is_match(&json)] == [true, false, false],
      "regular-expression rules must evaluate the normalized complete URL",
    )?;
    ensure(
      AssociationRule::regex("[").is_err(),
      "an invalid association regular expression must retain its typed compilation failure",
    )?;

    let exact = AssociationRule::Url(toml.clone());
    ensure(
      [exact.is_match(&toml), exact.is_match(&other)] == [true, false],
      "URL association rules must match exactly one document identity",
    )
  }

  #[test]
  fn association_priority_ties_and_mutations_have_deterministic_ownership() -> Result<(), TestFailure> {
    let associations = associations(TestEnvironment::default())?;
    associations.clear();
    ensure(associations.read().is_empty(), "clearing associations must remove every rule")?;

    let document_url = url("file:///workspace/document.toml")?;
    for (name, schema_url, rank) in [
      ("lower", "https://example.com/lower.json", priority::CONFIG),
      ("first-high", "https://example.com/first-high.json", priority::MAX),
      ("last-high", "https://example.com/last-high.json", priority::MAX),
    ] {
      associations.add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
        meta:     json!({ "name": name, "source": source::MANUAL }),
        url:      url(schema_url)?,
        priority: rank,
      });
    }
    associations.add(AssociationRule::Url(url("file:///workspace/unrelated.toml")?), SchemaAssociation {
      meta:     json!({ "name": "unrelated", "source": source::CONFIG }),
      url:      url("https://example.com/unrelated.json")?,
      priority: priority::MAX,
    });
    let selected = ensure_some(
      associations.association_for(&document_url),
      "the exact document must select one association",
    )?;
    ensure(
      association_metadata(&selected, "name") == Some("last-high"),
      "the highest priority must win and the later rule must break an equal-priority tie",
    )?;
    ensure(
      associations.association_for(&url("file:///workspace/missing.toml")?).is_none(),
      "a document with no matching rule must not fabricate an association",
    )?;

    let shared = associations.clone();
    shared.retain(|entry| association_metadata(&entry.1, "source") == Some(source::MANUAL));
    ensure_eq(
      &associations.read().len(),
      &3,
      "retaining through a clone must mutate the shared association set",
    )?;
    shared.clear();
    ensure(
      associations.read().is_empty(),
      "clearing through a clone must be visible to every shared holder",
    )?;
    ensure_result(
      associations.add_builtins(),
      "the built-in association must be reinstallable after a clear",
    )?;
    let built_in = ensure_some(
      associations.association_for(&url("file:///workspace/taplo.toml")?),
      "the Taplo configuration filename must match the built-in rule",
    )?;
    ensure(
      (association_metadata(&built_in, "source"), built_in.priority) == (Some(source::BUILTIN), priority::BUILTIN),
      "the rebuilt rule must retain its built-in ownership and priority",
    )
  }

  #[test]
  fn document_schema_locations_resolve_by_source_and_fail_atomically() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let associations = associations(environment)?;
    let document_url = url("file:///workspace/document.toml")?;
    let both = document("#:schema ./directive.json\n\"$schema\" = \"./field.json\"\n")?;
    ensure_result(
      associations.add_from_document(&document_url, &both),
      "relative directive and schema-field locations must resolve",
    )?;
    ensure_source_count(
      &associations,
      source::DIRECTIVE,
      1,
      "the document directive must own one exact URL rule",
    )?;
    ensure_source_count(
      &associations,
      source::SCHEMA_FIELD,
      1,
      "the document schema field must own one exact URL rule",
    )?;
    let selected = ensure_some(
      associations.association_for(&document_url),
      "the document must select one of its explicit schema locations",
    )?;
    ensure_eq(
      &selected.url.as_str(),
      &"file:///workspace/directive.json",
      "the higher-priority directive must win after document-relative resolution",
    )?;

    let absolute = document("#:schema /schemas/absolute.json\nvalue = 1\n")?;
    ensure_result(
      associations.add_from_document(&document_url, &absolute),
      "an absolute host path directive must resolve through the environment",
    )?;
    let absolute_selected = ensure_some(
      associations.association_for(&document_url),
      "the absolute directive must remain associated with its document",
    )?;
    ensure_eq(
      &absolute_selected.url.as_str(),
      &"file:///schemas/absolute.json",
      "an absolute host path directive must become a file URL",
    )?;

    let empty = document("#:schema \nvalue = 1\n")?;
    ensure(
      matches!(
        associations.add_from_document(&document_url, &empty),
        Err(AssociationError::EmptyDirective)
      ),
      "an empty directive must retain its typed failure",
    )?;
    let after_failure = ensure_some(
      associations.association_for(&document_url),
      "a rejected refresh must retain the prior document association",
    )?;
    ensure_eq(
      &after_failure.url.as_str(),
      &"file:///schemas/absolute.json",
      "a rejected document refresh must not partially replace prior ownership",
    )?;

    let non_relative_field = document("\"$schema\" = \"schema.json\"\n")?;
    ensure(
      matches!(
        associations.add_from_document(&document_url, &non_relative_field),
        Err(AssociationError::InvalidSchemaLocation {
          source_name: "`$schema` field",
          ..
        })
      ),
      "a non-URL schema field without an explicit relative prefix must be rejected",
    )?;
    let non_string = document("\"$schema\" = 1\n")?;
    ensure_result(
      associations.add_from_document(&document_url, &non_string),
      "a non-string schema field must be ignored",
    )?;
    ensure(
      associations.association_for(&document_url).is_none(),
      "a document without a supported directive or string schema field must clear stale document ownership",
    )?;

    let body_directive = document("value = 1\n#:schema https://example.com/body.json\n")?;
    ensure_result(
      associations.add_from_document(&document_url, &body_directive),
      "a body comment that resembles a directive must remain valid TOML",
    )?;
    ensure(
      associations.association_for(&document_url).is_none(),
      "only header comments may establish schema directives",
    )
  }

  #[test]
  fn catalog_shapes_transform_match_and_reject_invalid_metadata() -> Result<(), TestFailure> {
    let catalog_url = url("https://example.com/catalog.json")?;
    let schema_url = url("https://example.com/schema.json")?;
    let mut schema_store = ensure_ok(
      serde_json::from_value::<SchemaCatalog>(json!({
        "$schema": SCHEMA_STORE_CATALOG_SCHEMA_URL,
        "schemas": [{
          "name": "Schema Store",
          "description": "store entry",
          "url": schema_url,
          "fileMatch": ["*.json", "**/*.toml"],
          "versions": {}
        }]
      })),
      "a schema-store catalog must decode from its marker",
    )?;
    schema_store.transform_paths();
    let schema_store_replacements = ensure_result(
      catalog_replacements(&catalog_url, &schema_store),
      "a normalized schema-store catalog must compile its glob rule",
    )?;
    let schema_store_entry = ensure_some(
      schema_store_replacements.first(),
      "the schema-store catalog must produce one association",
    )?;
    ensure(
      (
        schema_store_replacements.len(),
        schema_store_entry.0.is_match(&url("file:///workspace/deep/value.json")?),
        schema_store_entry.0.is_match(&url("file:///workspace/deep/value.toml")?),
        association_metadata(&schema_store_entry.1, "name"),
        association_metadata(&schema_store_entry.1, "catalog_url"),
      ) == (1, true, true, Some("Schema Store"), Some(catalog_url.as_str())),
      "schema-store normalization must match original shallow globs at every directory depth and retain catalog metadata",
    )?;

    let taplo_catalog = ensure_ok(
      serde_json::from_value::<SchemaCatalog>(json!({
        "schemas": [{
          "title": "Taplo",
          "description": "taplo entry",
          "url": schema_url,
          "urlHash": "",
          "authors": [],
          "version": null,
          "patterns": [r".*\\.toml$", r".*\\.tml$"]
        }]
      })),
      "a Taplo catalog must decode without the schema-store marker",
    )?;
    let taplo_before_transform = ensure_ok(
      serde_json::to_value(&taplo_catalog),
      "the Taplo catalog must serialize before path normalization",
    )?;
    let mut transformed_taplo = taplo_catalog.clone();
    transformed_taplo.transform_paths();
    let taplo_after_transform = ensure_ok(
      serde_json::to_value(&transformed_taplo),
      "the Taplo catalog must serialize after path normalization",
    )?;
    ensure_eq(
      &taplo_after_transform,
      &taplo_before_transform,
      "schema-store path normalization must leave Taplo regular expressions unchanged",
    )?;
    let taplo_replacements = ensure_result(
      catalog_replacements(&catalog_url, &taplo_catalog),
      "a Taplo catalog must compile every regular expression",
    )?;
    ensure_eq(
      &taplo_replacements.len(),
      &2,
      "each Taplo catalog regular expression must produce an independently matchable association",
    )?;

    let invalid_store = ensure_ok(
      serde_json::from_value::<SchemaCatalog>(json!({
        "$schema": SCHEMA_STORE_CATALOG_SCHEMA_URL,
        "schemas": [{
          "name": "invalid glob",
          "description": "",
          "url": schema_url,
          "fileMatch": ["["],
          "versions": {}
        }]
      })),
      "an invalid schema-store glob remains catalog data until rule compilation",
    )?;
    ensure(
      matches!(
        catalog_replacements(&catalog_url, &invalid_store),
        Err(AssociationError::Glob { .. })
      ),
      "an invalid schema-store glob must retain its typed catalog context",
    )?;
    let invalid_taplo = ensure_ok(
      serde_json::from_value::<SchemaCatalog>(catalog(&schema_url, "invalid regex", "[")),
      "an invalid Taplo regular expression remains catalog data until rule compilation",
    )?;
    ensure(
      matches!(
        catalog_replacements(&catalog_url, &invalid_taplo),
        Err(AssociationError::Regex { .. })
      ),
      "an invalid Taplo regular expression must retain its typed catalog context",
    )?;

    let marker = ensure_ok(
      serde_json::to_value(SchemaStoreCatalogSchema),
      "the schema-store marker must serialize",
    )?;
    ensure_eq(
      &marker,
      &json!(SCHEMA_STORE_CATALOG_SCHEMA_URL),
      "the schema-store marker must serialize to its exact identifying URL",
    )?;
    ensure(
      serde_json::from_value::<SchemaStoreCatalogSchema>(json!("https://example.com/other.json")).is_err(),
      "a different schema URL must not decode as a schema-store catalog marker",
    )
  }

  #[test]
  fn config_associations_replace_transactionally_and_preserve_other_sources() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let associations = associations(environment.clone())?;
    let global_url = url("https://example.com/global.json")?;
    let rule_url = url("https://example.com/rule.json")?;
    let document_url = url("file:///workspace/match.toml")?;
    let manual_url = url("https://example.com/manual.json")?;
    associations.add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
      meta:     json!({ "source": source::MANUAL }),
      url:      manual_url.clone(),
      priority: priority::MAX,
    });

    let mut first = Config {
      global_options: schema_options(Some(global_url), Some(true)),
      rule: Vec::from([
        schema_rule("**/match.toml", None, Some(rule_url), Some(true)),
        schema_rule(
          "**/match.toml",
          Some(Vec::from(["nested".into()])),
          Some(url("https://example.com/key-scoped.json")?),
          Some(true),
        ),
        schema_rule("**/match.toml", None, None, Some(true)),
      ]),
      ..Config::default()
    };
    associations.add_from_config(&first);
    ensure_source_count(
      &associations,
      source::CONFIG,
      0,
      "an unprepared configuration must not fabricate file matchers or document associations",
    )?;
    ensure_result(
      first.prepare(&environment, Path::new("/workspace")),
      "the first association config must prepare",
    )?;
    associations.add_from_config(&first);
    ensure_source_count(
      &associations,
      source::CONFIG,
      2,
      "only enabled URL-bearing global and file rules may create associations",
    )?;
    let selected = ensure_some(associations.association_for(&document_url), "the document must have an association")?;
    ensure_eq(
      &selected.url.as_str(),
      &manual_url.as_str(),
      "a preserved higher-priority manual association must remain selected",
    )?;

    let mut disabled = Config {
      global_options: schema_options(Some(url("https://example.com/disabled.json")?), Some(false)),
      ..Config::default()
    };
    ensure_result(
      disabled.prepare(&environment, Path::new("/workspace")),
      "the disabling config must prepare",
    )?;
    associations.add_from_config(&disabled);
    ensure_source_count(
      &associations,
      source::CONFIG,
      0,
      "disabling config must remove stale config-derived associations",
    )?;
    ensure_source_count(
      &associations,
      source::MANUAL,
      1,
      "transactional config replacement must preserve manual associations",
    )
  }

  #[test]
  fn document_refresh_replaces_only_document_owned_sources() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let associations = associations(environment)?;
    let document_url = url("file:///workspace/document.toml")?;
    associations.add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
      meta:     json!({ "source": source::MANUAL }),
      url:      url("https://example.com/manual.json")?,
      priority: priority::MAX,
    });

    add_document_association(
      &associations,
      &document_url,
      "#:schema https://example.com/directive.json\nvalue = 1\n",
    )?;
    ensure_source_count(
      &associations,
      source::DIRECTIVE,
      1,
      "a schema directive must create one document-owned association",
    )?;
    ensure_source_count(
      &associations,
      source::MANUAL,
      1,
      "adding a directive must preserve a manual URL association",
    )?;

    add_document_association(&associations, &document_url, "\"$schema\" = \"https://example.com/field.json\"\n")?;
    ensure_source_count(
      &associations,
      source::DIRECTIVE,
      0,
      "refreshing the document must remove its previous directive",
    )?;
    ensure_source_count(
      &associations,
      source::SCHEMA_FIELD,
      1,
      "refreshing the document must install its current schema field",
    )?;

    associations.remove_from_document(&document_url);
    ensure_source_count(
      &associations,
      source::SCHEMA_FIELD,
      0,
      "removing document ownership must remove its schema field",
    )?;
    ensure_source_count(
      &associations,
      source::MANUAL,
      1,
      "removing document ownership must preserve the manual association",
    )
  }

  #[test]
  fn catalog_replacement_validates_the_complete_plan_before_commit() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let associations = associations(environment)?;
      associations.cache.set_cache_path(Some(PathBuf::from("/cache")));
      let first_catalog = url("https://example.com/first-catalog.json")?;
      let second_catalog = url("https://example.com/second-catalog.json")?;
      let invalid_catalog = url("https://example.com/invalid-catalog.json")?;
      let first_schema = url("https://example.com/first-schema.json")?;
      let second_schema = url("https://example.com/second-schema.json")?;
      let invalid_schema = url("https://example.com/invalid-schema.json")?;

      ensure_result(
        associations
          .cache
          .save(first_catalog.clone(), Arc::new(catalog(&first_schema, "first", r".*\.toml$")))
          .await,
        "the first catalog fixture must persist",
      )?;
      ensure_result(
        associations
          .cache
          .save(second_catalog.clone(), Arc::new(catalog(&second_schema, "second", r".*\.toml$")))
          .await,
        "the second catalog fixture must persist",
      )?;
      ensure_result(
        associations
          .cache
          .save(invalid_catalog.clone(), Arc::new(catalog(&invalid_schema, "invalid", "[")))
          .await,
        "the invalid catalog fixture must persist as transport data",
      )?;

      ensure_result(
        associations.replace_catalogs(from_ref(&first_catalog)).await,
        "the first catalog must install",
      )?;
      ensure_catalog_sources(
        &associations,
        [&first_catalog],
        "the first replacement must own the complete catalog association set",
      )?;

      let rejected = associations.replace_catalogs(&[second_catalog.clone(), invalid_catalog]).await;
      ensure(rejected.is_err(), "an invalid later catalog must reject the complete replacement")?;
      ensure_catalog_sources(
        &associations,
        [&first_catalog],
        "a rejected replacement must preserve the previously committed catalog set",
      )?;

      ensure_catalog_replacement_and_clear(
        &associations,
        &second_catalog,
        associations.replace_catalogs(from_ref(&second_catalog)),
        associations.replace_catalogs(&[]),
        [
          "the valid second catalog must replace the first",
          "a successful replacement must remove stale catalog ownership",
          "an empty catalog configuration must clear catalog ownership",
          "clearing catalogs must preserve no stale catalog association",
        ],
      )
      .await?;

      ensure_result(
        associations.add_from_catalog(&first_catalog).await,
        "the local single-catalog operation must install its owned entries",
      )?;
      ensure_catalog_sources(
        &associations,
        [&first_catalog],
        "single-catalog installation must own only its catalog URL",
      )
    })
  }

  /// Preserve catalog transaction semantics through the native concurrent operation family.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  #[test]
  fn concurrent_catalog_operations_validate_before_replacing_owned_entries() -> Result<(), TestFailure> {
    block_on(async {
      let first_catalog = url("https://example.com/first-concurrent-catalog.json")?;
      let second_catalog = url("https://example.com/second-concurrent-catalog.json")?;
      let invalid_catalog = url("https://example.com/invalid-concurrent-catalog.json")?;
      let first_schema = url("https://example.com/first-concurrent-schema.json")?;
      let second_schema = url("https://example.com/second-concurrent-schema.json")?;
      let invalid_schema = url("https://example.com/invalid-concurrent-schema.json")?;
      let concurrent = concurrent_associations(TestEnvironment::default())?;
      concurrent.cache.set_cache_path(Some(PathBuf::from("/concurrent-cache")));
      ensure_result(
        concurrent
          .cache
          .save(first_catalog.clone(), Arc::new(catalog(&first_schema, "first", r".*\.toml$")))
          .await,
        "the first concurrent catalog fixture must persist",
      )?;
      ensure_result(
        concurrent
          .cache
          .save(second_catalog.clone(), Arc::new(catalog(&second_schema, "second", r".*\.toml$")))
          .await,
        "the second concurrent catalog fixture must persist",
      )?;
      ensure_result(
        concurrent
          .cache
          .save(invalid_catalog.clone(), Arc::new(catalog(&invalid_schema, "invalid", "[")))
          .await,
        "the invalid concurrent catalog fixture must persist as transport data",
      )?;
      ensure_result(
        concurrent.add_from_catalog_concurrent(&first_catalog).await,
        "the first concurrent single-catalog operation must install its owned entries",
      )?;
      ensure_result(
        concurrent.add_from_catalog_concurrent(&second_catalog).await,
        "the second concurrent single-catalog operation must install its owned entries",
      )?;
      ensure_catalog_sources(
        &concurrent,
        [&first_catalog, &second_catalog],
        "single-catalog replacement must preserve entries owned by other catalogs",
      )?;

      let rejected = concurrent
        .replace_catalogs_concurrent(&[second_catalog.clone(), invalid_catalog])
        .await;
      ensure(
        rejected.is_err(),
        "an invalid concurrent catalog must reject the complete replacement",
      )?;
      ensure_catalog_sources(
        &concurrent,
        [&first_catalog, &second_catalog],
        "a rejected concurrent replacement must preserve every previously committed catalog",
      )?;

      ensure_catalog_replacement_and_clear(
        &concurrent,
        &second_catalog,
        concurrent.replace_catalogs_concurrent(from_ref(&second_catalog)),
        concurrent.replace_catalogs_concurrent(&[]),
        [
          "the concurrent complete-catalog operation must replace all catalog ownership",
          "concurrent complete-catalog replacement must remove stale catalog ownership",
          "the concurrent empty catalog set must clear catalog ownership",
          "the concurrent empty catalog set must preserve no stale entries",
        ],
      )
      .await
    })
  }

  /// Prefer current cached catalogs over host drift and retain typed malformed-data failures.
  #[test]
  fn current_catalog_cache_precedes_host_drift_and_rejects_malformed_data() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let current_associations = associations(environment.clone())?;
      current_associations.cache.set_cache_path(Some(PathBuf::from("/current-cache")));
      ensure(
        format!("{current_associations:?}").contains("rules: Some(1)"),
        "association debug output must expose rule cardinality without rendering rule contents",
      )?;

      let catalog_url = url("file:///workspace/catalog.json")?;
      let schema_url = url("https://example.com/schema.json")?;
      let first_catalog = catalog(&schema_url, "first", r".*\.toml$");
      let first_bytes = ensure_ok(serde_json::to_vec(&first_catalog), "the fresh catalog fixture must serialize")?;
      environment.insert_file("/workspace/catalog.json", first_bytes);
      ensure_result(
        current_associations.add_from_catalog(&catalog_url).await,
        "a fresh file catalog must load through the schema transport",
      )?;
      ensure(
        catalog_sources(&current_associations) == [catalog_url.to_string()],
        "a fresh transport load must install only the requested catalog ownership",
      )?;

      let replacement_schema = url("https://example.com/replacement.json")?;
      let replacement_bytes = ensure_ok(
        serde_json::to_vec(&catalog(&replacement_schema, "replacement", r".*\.json$")),
        "the replacement catalog fixture must serialize",
      )?;
      environment.insert_file("/workspace/catalog.json", replacement_bytes);
      ensure_result(
        current_associations.add_from_catalog(&catalog_url).await,
        "a current process-local catalog must remain loadable after host data changes",
      )?;
      let cached_selection = ensure_some(
        current_associations.association_for(&url("file:///workspace/value.toml")?),
        "the cached first catalog must still match its TOML document",
      )?;
      ensure_eq(
        &cached_selection.url.as_str(),
        &schema_url.as_str(),
        "a current process-local catalog must take precedence over later host drift",
      )?;
      ensure(
        current_associations
          .association_for(&url("file:///workspace/value.json")?)
          .is_none(),
        "host drift must not partially replace a current cached catalog",
      )?;

      let malformed_cached_url = url("https://example.com/malformed-cached.json")?;
      current_associations
        .cache
        .insert_memory(malformed_cached_url.clone(), Arc::new(json!({ "unexpected": true })));
      ensure(
        matches!(
          current_associations.add_from_catalog(&malformed_cached_url).await,
          Err(AssociationError::Catalog {
            ref url,
            ..
          }) if url == &malformed_cached_url
        ),
        "malformed process-local catalog data must retain its catalog URL and typed decoder failure",
      )
    })
  }

  /// Recover remote transport failure through stale disk state without erasing typed failures.
  #[test]
  fn stale_catalog_cache_recovers_transport_and_preserves_failures() -> Result<(), TestFailure> {
    block_on(async {
      let schema_url = url("https://example.com/schema.json")?;
      let stale_environment = TestEnvironment::default();
      let stale_associations = associations(stale_environment.clone())?;
      stale_associations.cache.set_cache_path(Some(PathBuf::from("/cache")));
      ensure_result(
        stale_associations.cache.set_expiration_times(Duration::ZERO, Duration::ZERO),
        "the stale-catalog fixture must configure immediate expiration",
      )?;
      let stale_catalog_url = url("https://example.com/stale-catalog.json")?;
      ensure_result(
        stale_associations
          .cache
          .save(stale_catalog_url.clone(), Arc::new(catalog(&schema_url, "stale", r".*\.toml$")))
          .await,
        "the stale catalog fixture must persist before expiration",
      )?;
      stale_environment.set_now(OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::seconds(1)));
      ensure_result(
        stale_associations.add_from_catalog(&stale_catalog_url).await,
        "an unavailable remote catalog must fall back to its expired disk entry",
      )?;
      let stale_selection = ensure_some(
        stale_associations.association_for(&url("file:///workspace/stale.toml")?),
        "the stale fallback catalog must install its association",
      )?;
      ensure_eq(
        &stale_selection.url.as_str(),
        &schema_url.as_str(),
        "stale recovery must preserve the catalog's original schema target",
      )?;

      let malformed_stale_url = url("https://example.com/malformed-stale.json")?;
      ensure_result(
        stale_associations
          .cache
          .save(malformed_stale_url.clone(), Arc::new(json!({ "unexpected": true })))
          .await,
        "the malformed stale fixture must persist as cache data",
      )?;
      stale_environment.set_now(OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::seconds(2)));
      ensure(
        matches!(
          stale_associations.add_from_catalog(&malformed_stale_url).await,
          Err(AssociationError::Catalog {
            ref url,
            ..
          }) if url == &malformed_stale_url
        ),
        "malformed stale catalog data must retain its URL and typed decoder failure",
      )?;

      let missing_url = url("https://example.com/missing-catalog.json")?;
      ensure(
        matches!(
          stale_associations.add_from_catalog(&missing_url).await,
          Err(AssociationError::Transport(_))
        ),
        "a transport failure without current or stale cache state must remain a typed transport error",
      )?;
      ensure(
        catalog_sources(&stale_associations) == [stale_catalog_url.to_string()],
        "failed catalog loads must preserve the last successfully committed catalog ownership",
      )
    })
  }
}
