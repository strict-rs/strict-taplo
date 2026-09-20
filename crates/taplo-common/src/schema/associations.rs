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
  use std::fmt::Debug;
  use std::path::Path;
  use std::path::PathBuf;
  use std::slice::from_ref;
  use std::sync::Arc;
  use std::time::Duration;

  use futures::executor::block_on;
  use serde_json::json;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;
  use taplo::dom::Node;
  use taplo::parser::Parse;
  use taplo::parser::ParseFailure;
  use taplo::parser::parse;
  use thiserror::Error;
  use time::OffsetDateTime;
  use url::ParseError;
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
  use crate::schema::cache::CacheError;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use crate::schema::transport::ConcurrentSchemaTransport;
  use crate::schema::transport::OfflineSchemaTransport;
  use crate::schema::transport::SchemaTransport;
  use crate::schema::transport::TransportError;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use crate::schema::transport::concurrent_http_client;
  use crate::test_support::TestEnvironment;

  /// Offline association service used by behavior tests.
  type TestAssociations = SchemaAssociations<OfflineSchemaTransport<TestEnvironment>>;

  /// Native failures while constructing association fixtures.
  #[derive(Debug, Error)]
  enum FixtureError {
    /// Fixture URL parsing failed.
    #[error(transparent)]
    Url(#[from] ParseError),
    /// Cache construction failed.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// Association construction failed.
    #[error(transparent)]
    Association(#[from] AssociationError),
    /// HTTP transport construction failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// Fixture syntax tree construction failed.
    #[error(transparent)]
    Parse(#[from] ParseFailure),
    /// Fixture syntax retained recoverable diagnostics.
    #[error(transparent)]
    Syntax(#[from] Box<PredicateFailure<Parse>>),
    /// Fixture serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
  }

  /// Parse a fixture URL without changing its native error.
  fn url(input: &str) -> Result<Url, ParseError> {
    Url::parse(input)
  }

  /// Construct schema options for a configuration fixture.
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

  /// Construct the existing offline service through its native initialization boundaries.
  fn associations(environment: TestEnvironment) -> Result<TestAssociations, FixtureError> {
    let transport = OfflineSchemaTransport::new(environment);
    let cache = Cache::new(transport.clone())?;
    Ok(SchemaAssociations::new(transport, cache)?)
  }

  /// Parse a clean document while retaining the complete parser on a syntax rejection.
  fn document(source_text: &str) -> Result<Node, FixtureError> {
    Ok(
      ensure_that(
        parse(source_text)?,
        "document association fixtures must have no syntax diagnostics",
        |parsed| parsed.diagnostics().is_empty(),
      )
      .map_err(Box::new)?
      .into_dom(),
    )
  }

  /// Clone the complete association state at an observable transaction boundary.
  fn snapshot<T: SchemaTransport>(associations: &SchemaAssociations<T>) -> Vec<(AssociationRule, SchemaAssociation)> {
    associations.read().clone()
  }

  /// Count one owner in an already retained association snapshot.
  fn source_count(entries: &[(AssociationRule, SchemaAssociation)], owner: &str) -> usize {
    entries
      .iter()
      .filter(|entry| association_metadata(&entry.1, "source") == Some(owner))
      .count()
  }

  /// Inspect catalog ownership without replacing the retained association subjects.
  fn catalog_sources(entries: &[(AssociationRule, SchemaAssociation)]) -> Vec<&str> {
    entries
      .iter()
      .filter(|entry| association_metadata(&entry.1, "source") == Some(source::CATALOG))
      .filter_map(|entry| association_metadata(&entry.1, "catalog_url"))
      .collect()
  }

  /// Construct one Taplo catalog containing a single pattern association.
  fn catalog(schema_url: &Url, title: &str, pattern: &str) -> serde_json::Value {
    json!({"schemas": [{"title": title, "description": "", "url": schema_url,
      "urlHash": "", "authors": [], "version": null, "patterns": [pattern]}]})
  }

  #[test]
  fn association_rules_match_each_family_and_reject_invalid_patterns() -> Result<(), impl Debug> {
    let observed = (|| {
      let documents = [
        url("file:///workspace/nested/example.toml")?,
        url("file:///workspace/nested/other.toml")?,
        url("file:///workspace/nested/example.json")?,
      ];
      let exact = AssociationRule::Url(documents[0].clone());
      Ok::<_, FixtureError>((
        documents,
        AssociationRule::glob("**/*.toml"),
        AssociationRule::glob("["),
        AssociationRule::regex(r"example\.toml$"),
        AssociationRule::regex("["),
        exact,
      ))
    })();
    ensure_that(
      observed,
      "glob, regex, and exact URL rules must preserve match polarities and native invalid-pattern failures",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual
          .1
          .as_ref()
          .is_ok_and(|glob| actual.0.each_ref().map(|document| glob.is_match(document)) == [true, true, false])
          && actual.2.is_err()
          && actual
            .3
            .as_ref()
            .is_ok_and(|regex| actual.0.each_ref().map(|document| regex.is_match(document)) == [true, false, false])
          && actual.4.is_err()
          && actual.0.each_ref().map(|document| actual.5.is_match(document)) == [true, false, false]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn association_priority_ties_and_mutations_have_deterministic_ownership() -> Result<(), impl Debug> {
    let observed = (|| {
      let service = associations(TestEnvironment::default())?;
      let document = url("file:///workspace/document.toml")?;
      let missing = url("file:///workspace/missing.toml")?;
      let built_in_url = url("file:///workspace/taplo.toml")?;
      let unrelated = url("file:///workspace/unrelated.toml")?;
      let entries = [
        ("lower", url("https://example.com/lower.json")?, priority::CONFIG),
        ("first-high", url("https://example.com/first-high.json")?, priority::MAX),
        ("last-high", url("https://example.com/last-high.json")?, priority::MAX),
      ];
      let unrelated_schema = url("https://example.com/unrelated.json")?;
      service.clear();
      let cleared = snapshot(&service);
      for (name, target, rank) in entries {
        service.add(AssociationRule::Url(document.clone()), SchemaAssociation {
          meta:     json!({"name": name, "source": source::MANUAL}),
          url:      target,
          priority: rank,
        });
      }
      service.add(AssociationRule::Url(unrelated), SchemaAssociation {
        meta:     json!({"name": "unrelated", "source": source::CONFIG}),
        url:      unrelated_schema,
        priority: priority::MAX,
      });
      let selected = service.association_for(&document);
      let absent = service.association_for(&missing);
      let shared = service.clone();
      shared.retain(|entry| association_metadata(&entry.1, "source") == Some(source::MANUAL));
      let retained = snapshot(&service);
      shared.clear();
      let shared_clear = snapshot(&service);
      let reinstalled = service.add_builtins();
      let builtin = service.association_for(&built_in_url);
      Ok::<_, FixtureError>((service, cleared, selected, absent, retained, shared_clear, reinstalled, builtin))
    })();
    ensure_that(
      observed,
      "priority ties and shared mutations must preserve deterministic association ownership",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.is_empty()
          && actual
            .2
            .as_ref()
            .is_some_and(|selected| association_metadata(selected, "name") == Some("last-high"))
          && actual.3.is_none()
          && actual.4.len() == 3
          && actual.5.is_empty()
          && actual.6.is_ok()
          && actual.7.as_ref().is_some_and(|builtin| {
            association_metadata(builtin, "source") == Some(source::BUILTIN) && builtin.priority == priority::BUILTIN
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_schema_locations_resolve_by_source_and_fail_atomically() -> Result<(), impl Debug> {
    let observed = (|| {
      let service = associations(TestEnvironment::default())?;
      let document_url = url("file:///workspace/document.toml")?;
      let sources = [
        "#:schema ./directive.json\n\"$schema\" = \"./field.json\"\n",
        "#:schema /schemas/absolute.json\nvalue = 1\n",
        "#:schema \nvalue = 1\n",
        "\"$schema\" = \"schema.json\"\n",
        "\"$schema\" = 1\n",
        "value = 1\n#:schema https://example.com/body.json\n",
      ];
      let documents = sources.into_iter().map(document).collect::<Result<Vec<_>, _>>()?;
      let states = documents
        .into_iter()
        .map(|root| {
          let added = service.add_from_document(&document_url, &root);
          (root, added, snapshot(&service), service.association_for(&document_url))
        })
        .collect::<Vec<_>>();
      Ok::<_, FixtureError>((service, states))
    })();
    ensure_that(
      observed,
      "document schema locations must preserve precedence, resolution, atomic rejection, and header-only ownership",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.as_slice().first_chunk::<6>().is_some_and(|states| {
          let [ref both, ref absolute, ref empty, ref non_relative, ref non_string, ref body] = *states;
          both.1.is_ok()
            && source_count(&both.2, source::DIRECTIVE) == 1
            && source_count(&both.2, source::SCHEMA_FIELD) == 1
            && both
              .3
              .as_ref()
              .is_some_and(|selected| selected.url.as_str() == "file:///workspace/directive.json")
            && absolute.1.is_ok()
            && absolute
              .3
              .as_ref()
              .is_some_and(|selected| selected.url.as_str() == "file:///schemas/absolute.json")
            && matches!(&empty.1, Err(AssociationError::EmptyDirective))
            && empty
              .3
              .as_ref()
              .is_some_and(|selected| selected.url.as_str() == "file:///schemas/absolute.json")
            && matches!(
              &non_relative.1,
              Err(AssociationError::InvalidSchemaLocation {
                source_name: "`$schema` field",
                ..
              })
            )
            && non_string.1.is_ok()
            && non_string.3.is_none()
            && body.1.is_ok()
            && body.3.is_none()
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn catalog_shapes_transform_match_and_reject_invalid_metadata() -> Result<(), impl Debug> {
    let observed = (|| {
      let catalog_url = url("https://example.com/catalog.json")?;
      let schema_url = url("https://example.com/schema.json")?;
      let documents = [
        url("file:///workspace/deep/value.json")?,
        url("file:///workspace/deep/value.toml")?,
      ];
      let mut store: SchemaCatalog = serde_json::from_value(json!({"$schema": SCHEMA_STORE_CATALOG_SCHEMA_URL,
        "schemas": [{"name": "Schema Store", "description": "store entry", "url": schema_url, "fileMatch": ["*.json", "**/*.toml"], "versions": {}}]}))?;
      store.transform_paths();
      let store_rules = catalog_replacements(&catalog_url, &store);
      let taplo: SchemaCatalog = serde_json::from_value(
        json!({"schemas": [{"title": "Taplo", "description": "taplo entry", "url": schema_url,
        "urlHash": "", "authors": [], "version": null, "patterns": [r".*\\.toml$", r".*\\.tml$"]}]}),
      )?;
      let before = serde_json::to_value(&taplo);
      let mut transformed = taplo.clone();
      transformed.transform_paths();
      let after = serde_json::to_value(&transformed);
      let taplo_rules = catalog_replacements(&catalog_url, &taplo);
      let invalid_store: SchemaCatalog = serde_json::from_value(json!({"$schema": SCHEMA_STORE_CATALOG_SCHEMA_URL,
        "schemas": [{"name": "invalid glob", "description": "", "url": schema_url, "fileMatch": ["["], "versions": {}}]}))?;
      let invalid_taplo: SchemaCatalog = serde_json::from_value(catalog(&schema_url, "invalid regex", "["))?;
      let rejected = [
        catalog_replacements(&catalog_url, &invalid_store),
        catalog_replacements(&catalog_url, &invalid_taplo),
      ];
      let marker = serde_json::to_value(SchemaStoreCatalogSchema);
      let wrong_marker = serde_json::from_value::<SchemaStoreCatalogSchema>(json!("https://example.com/other.json"));
      Ok::<_, FixtureError>((
        catalog_url,
        documents,
        (store, taplo, transformed, invalid_store, invalid_taplo),
        store_rules,
        before,
        after,
        taplo_rules,
        rejected,
        marker,
        wrong_marker,
      ))
    })();
    ensure_that(
      observed,
      "both catalog formats must preserve matching and metadata while rejecting invalid patterns and marker identities",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let Ok(ref rules) = actual.3 else {
          return false;
        };
        let [ref entry] = *rules.as_slice() else {
          return false;
        };
        actual.1.iter().all(|document| entry.0.is_match(document))
          && association_metadata(&entry.1, "name") == Some("Schema Store")
          && association_metadata(&entry.1, "catalog_url") == Some(actual.0.as_str())
          && actual
            .4
            .as_ref()
            .is_ok_and(|before| actual.5.as_ref().is_ok_and(|after| before == after))
          && actual.6.as_ref().is_ok_and(|taplo_rules| taplo_rules.len() == 2)
          && matches!(actual.7.first(), Some(Err(AssociationError::Glob { .. })))
          && matches!(actual.7.get(1), Some(Err(AssociationError::Regex { .. })))
          && actual
            .8
            .as_ref()
            .is_ok_and(|marker| marker == &json!(SCHEMA_STORE_CATALOG_SCHEMA_URL))
          && actual.9.is_err()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn config_associations_replace_transactionally_and_preserve_other_sources() -> Result<(), impl Debug> {
    let observed = (|| {
      let environment = TestEnvironment::default();
      let service = associations(environment.clone())?;
      let document_url = url("file:///workspace/match.toml")?;
      let manual_url = url("https://example.com/manual.json")?;
      let mut first = Config {
        global_options: schema_options(Some(url("https://example.com/global.json")?), Some(true)),
        rule: Vec::from([
          schema_rule("**/match.toml", None, Some(url("https://example.com/rule.json")?), Some(true)),
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
      let mut disabled = Config {
        global_options: schema_options(Some(url("https://example.com/disabled.json")?), Some(false)),
        ..Config::default()
      };
      service.add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
        meta:     json!({"source": source::MANUAL}),
        url:      manual_url.clone(),
        priority: priority::MAX,
      });
      service.add_from_config(&first);
      let unprepared = snapshot(&service);
      let prepared = first.prepare(&environment, Path::new("/workspace"));
      service.add_from_config(&first);
      let installed = snapshot(&service);
      let selected = service.association_for(&document_url);
      let disabled_prepared = disabled.prepare(&environment, Path::new("/workspace"));
      service.add_from_config(&disabled);
      let final_state = snapshot(&service);
      Ok::<_, FixtureError>((
        service, first, disabled, manual_url, unprepared, prepared, installed, selected, disabled_prepared, final_state,
      ))
    })();
    ensure_that(
      observed,
      "configuration replacement must require prepared matchers, ignore key scopes, and preserve manual ownership",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        source_count(&actual.4, source::CONFIG) == 0
          && actual.5.is_ok()
          && source_count(&actual.6, source::CONFIG) == 2
          && actual.7.as_ref().is_some_and(|selected| selected.url == actual.3)
          && actual.8.is_ok()
          && source_count(&actual.9, source::CONFIG) == 0
          && source_count(&actual.9, source::MANUAL) == 1
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_refresh_replaces_only_document_owned_sources() -> Result<(), impl Debug> {
    let observed = (|| {
      let service = associations(TestEnvironment::default())?;
      let document_url = url("file:///workspace/document.toml")?;
      let manual = url("https://example.com/manual.json")?;
      let directive = document("#:schema https://example.com/directive.json\nvalue = 1\n")?;
      let field = document("\"$schema\" = \"https://example.com/field.json\"\n")?;
      service.add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
        meta:     json!({"source": source::MANUAL}),
        url:      manual,
        priority: priority::MAX,
      });
      let first = service.add_from_document(&document_url, &directive);
      let first_state = snapshot(&service);
      let second = service.add_from_document(&document_url, &field);
      let second_state = snapshot(&service);
      service.remove_from_document(&document_url);
      let removed = snapshot(&service);
      Ok::<_, FixtureError>((service, directive, field, first, first_state, second, second_state, removed))
    })();
    ensure_that(
      observed,
      "document refresh and removal must replace only the document-owned directive and schema field",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.3.is_ok()
          && source_count(&actual.4, source::DIRECTIVE) == 1
          && source_count(&actual.4, source::MANUAL) == 1
          && actual.5.is_ok()
          && source_count(&actual.6, source::DIRECTIVE) == 0
          && source_count(&actual.6, source::SCHEMA_FIELD) == 1
          && source_count(&actual.7, source::SCHEMA_FIELD) == 0
          && source_count(&actual.7, source::MANUAL) == 1
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn catalog_replacement_validates_the_complete_plan_before_commit() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let service = associations(TestEnvironment::default())?;
      service.cache.set_cache_path(Some(PathBuf::from("/cache")));
      let catalogs = [
        url("https://example.com/first-catalog.json")?,
        url("https://example.com/second-catalog.json")?,
        url("https://example.com/invalid-catalog.json")?,
      ];
      let schemas = [
        url("https://example.com/first-schema.json")?,
        url("https://example.com/second-schema.json")?,
        url("https://example.com/invalid-schema.json")?,
      ];
      let [ref first, ref second, ref invalid] = catalogs;
      let mut saved = Vec::new();
      for (address, target, name, pattern) in [
        (&catalogs[0], &schemas[0], "first", r".*\.toml$"),
        (&catalogs[1], &schemas[1], "second", r".*\.toml$"),
        (&catalogs[2], &schemas[2], "invalid", "["),
      ] {
        saved.push(
          service
            .cache
            .save(address.clone(), Arc::new(catalog(target, name, pattern)))
            .await,
        );
      }
      let first_result = service.replace_catalogs(from_ref(first)).await;
      let first_state = snapshot(&service);
      let rejected = service.replace_catalogs(&[second.clone(), invalid.clone()]).await;
      let rejected_state = snapshot(&service);
      let replaced = service.replace_catalogs(from_ref(second)).await;
      let replaced_state = snapshot(&service);
      let cleared = service.replace_catalogs(&[]).await;
      let cleared_state = snapshot(&service);
      let single = service.add_from_catalog(first).await;
      let single_state = snapshot(&service);
      Ok::<_, FixtureError>((service, catalogs, schemas, saved, [
        (first_result, first_state),
        (rejected, rejected_state),
        (replaced, replaced_state),
        (cleared, cleared_state),
        (single, single_state),
      ]))
    });
    ensure_that(
      observed,
      "catalog replacement must validate the complete plan, preserve failed transactions, and support clearing and single-catalog \
       installation",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref installed, ref rejected, ref replaced, ref cleared, ref single] = actual.4;
        let [ref first, ref second, _] = actual.1;
        actual.3.iter().all(Result::is_ok)
          && installed.0.is_ok()
          && catalog_sources(&installed.1) == [first.as_str()]
          && rejected.0.is_err()
          && catalog_sources(&rejected.1) == [first.as_str()]
          && replaced.0.is_ok()
          && catalog_sources(&replaced.1) == [second.as_str()]
          && cleared.0.is_ok()
          && catalog_sources(&cleared.1).is_empty()
          && single.0.is_ok()
          && catalog_sources(&single.1) == [first.as_str()]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve catalog transaction semantics through the native concurrent operation family.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  #[test]
  fn concurrent_catalog_operations_validate_before_replacing_owned_entries() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let environment = TestEnvironment::default();
      let transport = ConcurrentSchemaTransport::new(environment.clone(), concurrent_http_client(&environment, Duration::from_secs(2))?);
      let cache = Cache::new(transport.clone())?;
      let service = SchemaAssociations::new(transport, cache)?;
      service.cache.set_cache_path(Some(PathBuf::from("/concurrent-cache")));
      let catalogs = [
        url("https://example.com/first-concurrent-catalog.json")?,
        url("https://example.com/second-concurrent-catalog.json")?,
        url("https://example.com/invalid-concurrent-catalog.json")?,
      ];
      let schemas = [
        url("https://example.com/first-concurrent-schema.json")?,
        url("https://example.com/second-concurrent-schema.json")?,
        url("https://example.com/invalid-concurrent-schema.json")?,
      ];
      let [ref first, ref second, ref invalid] = catalogs;
      let mut saved = Vec::new();
      for (address, target, name, pattern) in [
        (&catalogs[0], &schemas[0], "first", r".*\.toml$"),
        (&catalogs[1], &schemas[1], "second", r".*\.toml$"),
        (&catalogs[2], &schemas[2], "invalid", "["),
      ] {
        saved.push(
          service
            .cache
            .save(address.clone(), Arc::new(catalog(target, name, pattern)))
            .await,
        );
      }
      let installed = [
        service.add_from_catalog_concurrent(first).await,
        service.add_from_catalog_concurrent(second).await,
      ];
      let installed_state = snapshot(&service);
      let rejected = service.replace_catalogs_concurrent(&[second.clone(), invalid.clone()]).await;
      let rejected_state = snapshot(&service);
      let replaced = service.replace_catalogs_concurrent(from_ref(second)).await;
      let replaced_state = snapshot(&service);
      let cleared = service.replace_catalogs_concurrent(&[]).await;
      let cleared_state = snapshot(&service);
      Ok::<_, FixtureError>((
        service, catalogs, schemas, saved, installed, installed_state, rejected, rejected_state, replaced, replaced_state, cleared,
        cleared_state,
      ))
    });
    ensure_that(
      observed,
      "concurrent catalog operations must validate before replacing ownership and preserve prior catalogs after rejection",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.3.iter().all(Result::is_ok)
          && actual.4.iter().all(Result::is_ok)
          && catalog_sources(&actual.5) == [actual.1[0].as_str(), actual.1[1].as_str()]
          && actual.6.is_err()
          && catalog_sources(&actual.7) == [actual.1[0].as_str(), actual.1[1].as_str()]
          && actual.8.is_ok()
          && catalog_sources(&actual.9) == [actual.1[1].as_str()]
          && actual.10.is_ok()
          && catalog_sources(&actual.11).is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn current_catalog_cache_precedes_host_drift_and_rejects_malformed_data() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let environment = TestEnvironment::default();
      let service = associations(environment.clone())?;
      service.cache.set_cache_path(Some(PathBuf::from("/current-cache")));
      let catalog_url = url("file:///workspace/catalog.json")?;
      let schema_url = url("https://example.com/schema.json")?;
      let replacement_url = url("https://example.com/replacement.json")?;
      let malformed_url = url("https://example.com/malformed-cached.json")?;
      let documents = [url("file:///workspace/value.toml")?, url("file:///workspace/value.json")?];
      let first_bytes = serde_json::to_vec(&catalog(&schema_url, "first", r".*\.toml$"))?;
      let replacement_bytes = serde_json::to_vec(&catalog(&replacement_url, "replacement", r".*\.json$"))?;
      let debug = format!("{service:?}");
      environment.insert_file("/workspace/catalog.json", first_bytes.clone());
      let loaded = service.add_from_catalog(&catalog_url).await;
      let loaded_state = snapshot(&service);
      environment.insert_file("/workspace/catalog.json", replacement_bytes.clone());
      let reloaded = service.add_from_catalog(&catalog_url).await;
      let selections = documents.each_ref().map(|document| service.association_for(document));
      service
        .cache
        .insert_memory(malformed_url.clone(), Arc::new(json!({"unexpected": true})));
      let malformed = service.add_from_catalog(&malformed_url).await;
      Ok::<_, FixtureError>((
        service, catalog_url, schema_url, malformed_url, debug, first_bytes, replacement_bytes, loaded, loaded_state, reloaded, selections,
        malformed,
      ))
    });
    ensure_that(
      observed,
      "current catalog cache must outrank host drift and malformed cached data must retain its URL and decoder failure",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.4.contains("rules: Some(1)")
          && actual.7.is_ok()
          && catalog_sources(&actual.8) == [actual.1.as_str()]
          && actual.9.is_ok()
          && actual.10[0].as_ref().is_some_and(|selected| selected.url == actual.2)
          && actual.10[1].is_none()
          && matches!(&actual.11, Err(AssociationError::Catalog { url, .. }) if url == &actual.3)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn stale_catalog_cache_recovers_transport_and_preserves_failures() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let environment = TestEnvironment::default();
      let service = associations(environment.clone())?;
      service.cache.set_cache_path(Some(PathBuf::from("/cache")));
      let schema_url = url("https://example.com/schema.json")?;
      let stale_url = url("https://example.com/stale-catalog.json")?;
      let malformed_url = url("https://example.com/malformed-stale.json")?;
      let missing_url = url("https://example.com/missing-catalog.json")?;
      let document_url = url("file:///workspace/stale.toml")?;
      let policy = service.cache.set_expiration_times(Duration::ZERO, Duration::ZERO);
      let saved = service
        .cache
        .save(stale_url.clone(), Arc::new(catalog(&schema_url, "stale", r".*\.toml$")))
        .await;
      environment.set_now(OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::seconds(1)));
      let recovered = service.add_from_catalog(&stale_url).await;
      let selected = service.association_for(&document_url);
      let saved_malformed = service
        .cache
        .save(malformed_url.clone(), Arc::new(json!({"unexpected": true})))
        .await;
      environment.set_now(OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::seconds(2)));
      let malformed = service.add_from_catalog(&malformed_url).await;
      let missing = service.add_from_catalog(&missing_url).await;
      let final_state = snapshot(&service);
      Ok::<_, FixtureError>((
        service, schema_url, stale_url, malformed_url, policy, saved, recovered, selected, saved_malformed, malformed, missing, final_state,
      ))
    });
    ensure_that(
      observed,
      "stale recovery must retain its target while malformed and missing catalogs preserve typed failures and committed ownership",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.4.is_ok()
          && actual.5.is_ok()
          && actual.6.is_ok()
          && actual.7.as_ref().is_some_and(|selected| selected.url == actual.1)
          && actual.8.is_ok()
          && matches!(&actual.9, Err(AssociationError::Catalog { url, .. }) if url == &actual.3)
          && matches!(&actual.10, Err(AssociationError::Transport(_)))
          && catalog_sources(&actual.11) == [actual.2.as_str()]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
