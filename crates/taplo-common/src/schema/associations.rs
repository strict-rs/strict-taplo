use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use anyhow::anyhow;
use parking_lot::RwLock;
use parking_lot::RwLockReadGuard;
use regex::Regex;
use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Error;
use serde_json::Value;
use serde_json::json;
use tap::Tap;
use taplo::dom::Node;
use tokio::sync::Semaphore;
use url::Url;

use super::builtins;
use super::cache::Cache;
use crate::IndexMap;
use crate::config::Config;
use crate::config::SchemaOptions;
use crate::environment::Environment;
use crate::util::GlobRule;
use crate::util::normalize_str;

pub const DEFAULT_CATALOGS: &[&str] = &["https://json.schemastore.org/api/json/catalog.json"];

pub mod priority {
  pub const BUILTIN: usize = 10;
  pub const CATALOG: usize = 25;
  pub const CONFIG: usize = 50;
  pub const CONFIG_RULE: usize = 51;
  pub const LSP_CONFIG: usize = 60;
  pub const SCHEMA_FIELD: usize = 70;
  pub const DIRECTIVE: usize = 75;
  pub const MAX: usize = usize::MAX;
}

pub mod source {
  pub const BUILTIN: &str = "builtin";
  pub const CATALOG: &str = "catalog";
  pub const CONFIG: &str = "config";
  pub const LSP_CONFIG: &str = "lsp_config";
  pub const MANUAL: &str = "manual";
  pub const SCHEMA_FIELD: &str = "$schema";
  pub const DIRECTIVE: &str = "directive";
}

#[derive(Clone)]
pub struct SchemaAssociations<E: Environment> {
  concurrent_requests: Arc<Semaphore>,
  http:                Option<reqwest::Client>,
  env:                 E,
  associations:        Arc<RwLock<Vec<(AssociationRule, SchemaAssociation)>>>,
  cache:               Cache<E>,
}

impl<E: Environment> SchemaAssociations<E> {
  pub(crate) fn new(env: E, cache: Cache<E>, http: Option<reqwest::Client>) -> Self {
    let this = Self {
      concurrent_requests: Arc::new(Semaphore::new(10)),
      cache,
      env,
      http,
      associations: Default::default(),
    };
    this.add_builtins();
    this
  }

  pub fn add(&self, rule: AssociationRule, assoc: SchemaAssociation) {
    self.associations.write().push((rule, assoc));
  }

  pub fn retain(&self, f: impl Fn(&(AssociationRule, SchemaAssociation)) -> bool) {
    self.associations.write().retain(f);
  }

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

  pub fn add_builtins(&self) {
    self.retain(|(_, assoc)| assoc.meta["source"] != source::BUILTIN);

    let regex = Regex::new(r".*\.?taplo\.toml$");
    let url = builtins::TAPLO_CONFIG_URL.parse();
    match (regex, url) {
      (Ok(regex), Ok(url)) => self
        .associations
        .write()
        .push((AssociationRule::Regex(regex), SchemaAssociation {
          url,
          meta: json!({
              "name": "Taplo",
              "description": "Taplo configuration file.",
              "source": source::BUILTIN
          }),
          priority: priority::BUILTIN,
        })),
      (Err(error), _) => tracing::error!(%error, "invalid built-in association regex"),
      (_, Err(error)) => tracing::error!(%error, "invalid built-in schema URL"),
    }
  }

  pub async fn add_from_catalog(&self, url: &Url) -> Result<(), anyhow::Error> {
    let index = self.load_catalog(url).await?;
    self.retain(|(_, association)| {
      association.meta["source"] != source::CATALOG || association.meta["catalog_url"].as_str() != Some(url.as_str())
    });
    match index {
      SchemaCatalog::SchemaStore(index) => {
        for schema in &index.schemas {
          match GlobRule::new(&schema.file_match, [] as [&str; 0]) {
            Ok(rule) => {
              self.associations.write().push((rule.into(), SchemaAssociation {
                url:      schema.url.clone(),
                meta:     json!({
                    "name": schema.name,
                    "description": schema.description,
                    "source": source::CATALOG,
                    "catalog_url": url,
                }),
                priority: priority::CATALOG,
              }));
            }
            Err(error) => {
              tracing::warn!(
                  %error,
                  schema_name = %schema.name,
                  source = %url,
                  "invalid glob pattern(s)"
              );
            }
          }
        }
      }
      SchemaCatalog::Taplo(index) => {
        for schema in &index.schemas {
          for pattern in &schema.extra.patterns {
            let regex = match Regex::new(pattern) {
              Ok(pat) => pat,
              Err(error) => {
                tracing::warn!(
                    %error,
                    pattern = %pattern,
                    schema_name = %schema.title,
                    "invalid regex pattern"
                );
                continue;
              }
            };

            self.associations.write().push((regex.into(), SchemaAssociation {
              url:      schema.url.clone(),
              meta:     json!({
                  "name": schema.title,
                  "description": schema.description,
                  "source": source::CATALOG,
                  "catalog_url": url,
              }),
              priority: priority::CATALOG,
            }));
          }
        }
      }
    }
    Ok(())
  }

  /// Adds the schema from either a directive, or a `$schema` key in the root.
  pub fn add_from_document(&self, doc_url: &Url, root: &Node) {
    self.remove_from_document(doc_url);

    for comment in root.header_comments() {
      if let Some("schema") = comment.directive() {
        let value = comment.value();

        if value.is_empty() {
          tracing::warn!("empty schema directive");
          continue;
        }

        let schema_url: Url = match value.parse() {
          Ok(url) => url,
          Err(error) => {
            tracing::debug!(%error, "invalid url in directive, assuming file path instead");

            if self.env.is_absolute(Path::new(value)) {
              match format!("file://{value}").parse() {
                Ok(u) => u,
                Err(error) => {
                  tracing::error!(%error, "invalid schema directive");
                  continue;
                }
              }
            } else {
              match doc_url.join(value) {
                Ok(u) => u,
                Err(error) => {
                  tracing::error!(%error, "invalid schema directive");
                  continue;
                }
              }
            }
          }
        };

        self
          .associations
          .write()
          .push((AssociationRule::Url(doc_url.clone()), SchemaAssociation {
            url:      schema_url,
            priority: priority::DIRECTIVE,
            meta:     json!({ "source": source::DIRECTIVE }),
          }));
        break;
      }
    }

    if let Node::Str(s) = root.get("$schema") {
      let schema_url: Url = if s.value().starts_with('.') {
        match doc_url.join(s.value()) {
          Ok(s) => s,
          Err(error) => {
            tracing::error!(%error, "invalid schema url or path given in the `$schema` field");
            return;
          }
        }
      } else {
        match s.value().parse() {
          Ok(s) => s,
          Err(error) => {
            tracing::error!(%error, "invalid schema url or path given in the `$schema` field");
            return;
          }
        }
      };

      self
        .associations
        .write()
        .push((AssociationRule::Url(doc_url.clone()), SchemaAssociation {
          url:      schema_url,
          priority: priority::SCHEMA_FIELD,
          meta:     json!({ "source": source::SCHEMA_FIELD }),
        }));
    }
  }

  /// Remove only directive and `$schema` associations owned by one document.
  pub fn remove_from_document(&self, doc_url: &Url) {
    self.retain(|(rule, association)| match rule {
      AssociationRule::Url(url) => {
        url != doc_url || (association.meta["source"] != source::DIRECTIVE && association.meta["source"] != source::SCHEMA_FIELD)
      }
      _ => true,
    });
  }

  pub fn add_from_config(&self, config: &Config) {
    self.retain(|(_, association)| association.meta["source"] != source::CONFIG);

    for rule in &config.rule {
      if rule.keys.is_some() {
        continue;
      }
      let Some(file_rule) = rule.file_rule.clone() else {
        continue;
      };

      if let Some(association) = rule
        .options
        .schema
        .as_ref()
        .and_then(|options| config_association(options, priority::CONFIG_RULE))
      {
        self.associations.write().push((file_rule.into(), association));
      }
    }

    let Some(file_rule) = config.file_rule.clone() else {
      return;
    };

    if let Some(association) = config
      .global_options
      .schema
      .as_ref()
      .and_then(|options| config_association(options, priority::CONFIG))
    {
      self.associations.write().push((file_rule.into(), association));
    }
  }

  pub fn association_for(&self, file: &Url) -> Option<SchemaAssociation> {
    self
      .associations
      .read()
      .iter()
      .filter_map(|(rule, assoc)| if rule.is_match(file) { Some(assoc.clone()) } else { None })
      .max_by_key(|assoc| assoc.priority)
      .tap(|s| {
        if let Some(schema_association) = s {
          tracing::debug!(
              schema.url = %schema_association.url,
              schema.name = schema_association.meta["name"].as_str().unwrap_or(""),
              schema.source = schema_association.meta["source"].as_str().unwrap_or(""),
              "found schema association"
          );
        }
      })
  }

  async fn load_catalog(&self, index_url: &Url) -> Result<SchemaCatalog, anyhow::Error> {
    if let Ok(s) = self.cache.load(index_url, false).await {
      return Ok(serde_json::from_value((*s).clone())?);
    }

    let mut index = match self.fetch_external(index_url).await {
      Ok(idx) => idx,
      Err(error) => {
        tracing::warn!(?error, "failed to fetch catalog");
        if let Ok(s) = self.cache.load(index_url, true).await {
          return Ok(serde_json::from_value((*s).clone())?);
        }
        return Err(error);
      }
    };

    index.transform_paths();

    if let Err(error) = self
      .cache
      .save_if_configured(index_url.clone(), Arc::new(serde_json::to_value(&index)?))
      .await
    {
      tracing::warn!(%error, "failed to cache index");
    }

    Ok(index)
  }

  async fn fetch_external(&self, index_url: &Url) -> Result<SchemaCatalog, anyhow::Error> {
    let _permit = self.concurrent_requests.acquire().await?;
    match index_url.scheme() {
      "http" | "https" => {
        let Some(http) = &self.http else {
          return Err(anyhow!("HTTP schema transport is unavailable for catalog `{index_url}`"));
        };
        Ok(http.get(index_url.clone()).send().await?.json().await?)
      }
      "file" => Ok(serde_json::from_slice(
        &self
          .env
          .read_file(
            self
              .env
              .to_file_path_normalized(index_url)
              .ok_or_else(|| anyhow!("invalid file path"))?
              .as_ref(),
          )
          .await?,
      )?),
      scheme => Err(anyhow!("the scheme `{scheme}` is not supported")),
    }
  }
}

#[derive(Clone)]
pub enum AssociationRule {
  Glob(GlobRule),
  Regex(Regex),
  Url(Url),
}

impl AssociationRule {
  pub fn glob(pattern: &str) -> Result<Self, anyhow::Error> {
    Ok(Self::Glob(GlobRule::new([pattern], &[] as &[&str])?))
  }

  pub fn regex(regex: &str) -> Result<Self, anyhow::Error> {
    Ok(Self::Regex(Regex::new(regex)?))
  }
}

impl From<Regex> for AssociationRule {
  fn from(v: Regex) -> Self {
    Self::Regex(v)
  }
}

impl From<GlobRule> for AssociationRule {
  fn from(v: GlobRule) -> Self {
    Self::Glob(v)
  }
}

impl AssociationRule {
  #[must_use]
  pub fn is_match(&self, url: &Url) -> bool {
    match self {
      // Glob associations typically come from config files
      // with a glob pattern that is an absolute file path
      // without a scheme.
      //
      // So in order to be a match, we need to
      // strip the scheme from the URL.
      AssociationRule::Glob(g) => g.is_match(&*normalize_str(
        url
          .as_str()
          .strip_prefix(url.scheme())
          .and_then(|without_scheme| without_scheme.strip_prefix("://"))
          .unwrap_or(url.path()),
      )),
      AssociationRule::Regex(r) => r.is_match(&normalize_str(url.as_str())),
      AssociationRule::Url(u) => u == url,
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SchemaCatalog {
  SchemaStore(SchemaStoreCatalog),
  Taplo(TaploSchemaCatalog),
}

impl SchemaCatalog {
  fn transform_paths(&mut self) {
    if let SchemaCatalog::SchemaStore(index) = self {
      for s in &mut index.schemas {
        for fm in &mut s.file_match {
          if !fm.starts_with("**/") {
            *fm = String::from("**/") + fm.as_str();
          }
        }
      }
    }
  }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TaploSchemaCatalog {
  pub schemas: Vec<TaploSchemaMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaploSchemaMeta {
  #[serde(default)]
  pub title:       String,
  #[serde(default)]
  pub description: String,
  pub url:         Url,
  pub url_hash:    String,

  #[serde(flatten)]
  pub extra: TaploSchemaExtraInfo,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaploSchemaExtraInfo {
  pub authors:  Vec<String>,
  pub version:  Option<Version>,
  pub patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaStoreCatalog {
  #[serde(rename = "$schema")]
  pub schema:  SchemaStoreCatalogSchema,
  pub schemas: Vec<SchemaStoreSchemaMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaStoreSchemaMeta {
  #[serde(default)]
  pub name:        String,
  #[serde(default)]
  pub description: String,
  pub url:         Url,
  #[serde(default)]
  pub file_match:  Vec<String>,
  #[serde(default)]
  pub versions:    IndexMap<String, Url>,
}

pub const SCHEMA_STORE_CATALOG_SCHEMA_URL: &str = "https://json.schemastore.org/schema-catalog.json";

#[derive(Debug, Clone, Copy)]
pub struct SchemaStoreCatalogSchema;

impl<'de> Deserialize<'de> for SchemaStoreCatalogSchema {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let s = Cow::<'static, str>::deserialize(deserializer)?;

    if s != SCHEMA_STORE_CATALOG_SCHEMA_URL {
      return Err(Error::custom(format!("expected $schema to be {SCHEMA_STORE_CATALOG_SCHEMA_URL}")));
    }

    Ok(SchemaStoreCatalogSchema)
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

#[derive(Debug, Clone)]
pub struct SchemaAssociation {
  pub meta:     Value,
  pub url:      Url,
  pub priority: usize,
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::parser::parse;
  use url::Url;

  use super::AssociationRule;
  use super::SchemaAssociation;
  use super::SchemaAssociations;
  use super::priority;
  use super::source;
  use crate::config::Config;
  use crate::config::Options;
  use crate::config::Rule;
  use crate::config::SchemaOptions;
  use crate::schema::cache::Cache;
  use crate::test_support::TestEnvironment;
  use crate::test_support::ensure_anyhow;

  fn url(value: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(value), "the association fixture URL must parse")
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

  fn source_count(associations: &SchemaAssociations<TestEnvironment>, expected_source: &str) -> usize {
    associations
      .read()
      .iter()
      .filter(|(_, association)| association.meta["source"] == expected_source)
      .count()
  }

  #[test]
  fn config_associations_replace_transactionally_and_preserve_other_sources() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let associations = SchemaAssociations::new(environment.clone(), Cache::new(environment.clone()), None);
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
        Rule {
          include: Some(Vec::from(["**/match.toml".into()])),
          options: schema_options(Some(rule_url), Some(true)),
          ..Rule::default()
        },
        Rule {
          include: Some(Vec::from(["**/match.toml".into()])),
          keys: Some(Vec::from(["nested".into()])),
          options: schema_options(Some(url("https://example.com/key-scoped.json")?), Some(true)),
          ..Rule::default()
        },
        Rule {
          include: Some(Vec::from(["**/match.toml".into()])),
          options: schema_options(None, Some(true)),
          ..Rule::default()
        },
      ]),
      ..Config::default()
    };
    ensure_anyhow(
      first.prepare(&environment, Path::new("/workspace")),
      "the first association config must prepare",
    )?;
    associations.add_from_config(&first);
    ensure_eq(
      &source_count(&associations, source::CONFIG),
      &2,
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
    ensure_anyhow(
      disabled.prepare(&environment, Path::new("/workspace")),
      "the disabling config must prepare",
    )?;
    associations.add_from_config(&disabled);
    ensure_eq(
      &source_count(&associations, source::CONFIG),
      &0,
      "disabling config must remove stale config-derived associations",
    )?;
    ensure_eq(
      &source_count(&associations, source::MANUAL),
      &1,
      "transactional config replacement must preserve manual associations",
    )
  }

  #[test]
  fn document_refresh_replaces_only_document_owned_sources() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let associations = SchemaAssociations::new(environment.clone(), Cache::new(environment), None);
    let document_url = url("file:///workspace/document.toml")?;
    associations.add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
      meta:     json!({ "source": source::MANUAL }),
      url:      url("https://example.com/manual.json")?,
      priority: priority::MAX,
    });

    let directive = parse("#:schema https://example.com/directive.json\nvalue = 1\n");
    ensure(directive.errors.is_empty(), "the directive fixture must parse cleanly")?;
    associations.add_from_document(&document_url, &directive.into_dom());
    ensure_eq(
      &source_count(&associations, source::DIRECTIVE),
      &1,
      "a schema directive must create one document-owned association",
    )?;
    ensure_eq(
      &source_count(&associations, source::MANUAL),
      &1,
      "adding a directive must preserve a manual URL association",
    )?;

    let schema_field = parse("\"$schema\" = \"https://example.com/field.json\"\n");
    ensure(schema_field.errors.is_empty(), "the schema-field fixture must parse cleanly")?;
    associations.add_from_document(&document_url, &schema_field.into_dom());
    ensure_eq(
      &source_count(&associations, source::DIRECTIVE),
      &0,
      "refreshing the document must remove its previous directive",
    )?;
    ensure_eq(
      &source_count(&associations, source::SCHEMA_FIELD),
      &1,
      "refreshing the document must install its current schema field",
    )?;

    associations.remove_from_document(&document_url);
    ensure_eq(
      &source_count(&associations, source::SCHEMA_FIELD),
      &0,
      "removing document ownership must remove its schema field",
    )?;
    ensure_eq(
      &source_count(&associations, source::MANUAL),
      &1,
      "removing document ownership must preserve the manual association",
    )
  }
}
