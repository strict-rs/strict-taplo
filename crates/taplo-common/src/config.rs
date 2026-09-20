use std::path::Path;
use std::path::PathBuf;
use std::slice::from_ref;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use taplo::formatter;
use thiserror::Error;
use url::Url;

use crate::HashMap;
use crate::environment::Environment;
use crate::environment::EnvironmentError;
use crate::util::GlobRule;
use crate::util::GlobRuleError;
use crate::util::Normalize as _;

/// Supported configuration filenames, in search-precedence order.
pub const CONFIG_FILE_NAMES: &[&str] = &[".taplo.toml", "taplo.toml"];

/// A typed failure while normalizing and preparing Taplo configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
  /// A host path-model operation failed.
  #[error(transparent)]
  Environment(#[from] EnvironmentError),
  /// A configured include or exclude glob was invalid.
  #[error(transparent)]
  Glob(#[from] GlobRuleError),
  /// A local schema path could not be represented as a file URL.
  #[error("invalid schema path `{path}`")]
  InvalidSchemaPath {
    /// Rejected local path.
    path: PathBuf,
  },
  /// A normalized include or exclude path cannot be represented as a glob string.
  #[error("configuration path `{path}` is not valid Unicode")]
  PathUnicode {
    /// Rejected normalized path.
    path: PathBuf,
  },
}

/// The `taplo.toml` configuration model.
///
/// A configuration selects the files it applies to (`include`/`exclude`), carries the
/// options that apply to every selected file (`global_options`), and refines those options
/// per file and per document key through [`rule`](Self::rule) entries.
///
/// [`prepare`](Self::prepare) must run before any matching accessor: it resolves relative
/// globs and schema paths against a base directory and compiles the glob matchers the
/// accessors rely on.
#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
  /// Files to include.
  ///
  /// A list of Unix-like [glob](https://en.wikipedia.org/wiki/Glob_(programming)) path patterns.
  /// Globstars (`**`) are supported.
  ///
  /// Relative paths are **not** relative to the configuration file, but rather
  /// depends on the tool using the configuration.
  ///
  /// Omitting this property includes all files, **however an empty array will include none**.
  pub include: Option<Vec<String>>,

  /// Files to exclude (ignore).
  ///
  /// A list of Unix-like [glob](https://en.wikipedia.org/wiki/Glob_(programming)) path patterns.
  /// Globstars (`**`) are supported.
  ///
  /// Relative paths are **not** relative to the configuration file, but rather
  /// depends on the tool using the configuration.
  ///
  /// This has priority over `include`.
  pub exclude: Option<Vec<String>>,

  /// Rules are used to override configurations by path and keys.
  #[serde(default)]
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub rule: Vec<Rule>,

  /// Options applied to every included file before any matching [`Rule`] refines them.
  #[serde(flatten)]
  pub global_options: Options,

  /// Compiled `include`/`exclude` matcher, populated by [`Config::prepare`].
  ///
  /// [`Config::is_included`] reports no match while this is `None`.
  #[serde(skip)]
  pub file_rule: Option<GlobRule>,

  /// Settings passed through to plugins, keyed by plugin name.
  ///
  /// Taplo does not interpret the contents; each consuming tool reads the entry it owns.
  #[serde(default)]
  #[serde(skip_serializing_if = "Option::is_none")]
  pub plugins: Option<HashMap<String, Plugin>>,
}

impl Config {
  /// Prepare the configuration for further use.
  ///
  /// # Errors
  ///
  /// Returns [`ConfigError`] when a host path operation, glob expression, or
  /// schema path is invalid.
  pub fn prepare(&mut self, environment: &impl Environment, base: &Path) -> Result<(), ConfigError> {
    self.make_absolute(environment, base)?;

    let default_include = String::from("**/*.toml");
    let include = self.include.as_deref().unwrap_or_else(|| from_ref(&default_include));
    let exclude = self.exclude.as_deref().unwrap_or(&[]);

    self.file_rule = Some(GlobRule::new(include, exclude)?);

    for rule in &mut self.rule {
      rule.prepare(environment, base)?;
    }

    self.global_options.prepare(environment, base)?;

    Ok(())
  }

  /// Return whether this configuration applies to one file.
  ///
  /// Always `false` before [`Config::prepare`] has compiled the file rule.
  #[must_use]
  pub fn is_included(&self, path: &Path) -> bool {
    self.file_rule.as_ref().map_or_else(
      || {
        tracing::debug!("no file matches were set up");
        false
      },
      |rule| rule.is_match(path),
    )
  }

  /// Iterate the `[[rule]]` entries that match one file, in declaration order.
  ///
  /// Both key-scoped and whole-document rules are yielded; callers that only want one kind
  /// filter on [`Rule::keys`].
  #[must_use]
  pub fn rules_for<'r>(&'r self, path: &'r Path) -> impl DoubleEndedIterator<Item = &'r Rule> + Clone + 'r {
    self.rule.iter().filter(|rule| rule.is_included(path))
  }

  /// Apply this configuration's whole-document formatter options for one file.
  ///
  /// Global options are applied first, then every matching rule that is not key-scoped, so a
  /// later rule wins over an earlier one and any rule wins over the global options. Key-scoped
  /// rules are ignored here; [`Config::format_scopes`] owns those.
  pub fn update_format_options(&self, path: &Path, options: &mut formatter::Options) {
    if let Some(opts) = self.global_options.formatting.as_ref() {
      options.update(opts.clone());
    }

    for rule in self.rules_for(path) {
      if rule.keys.is_none()
        && let Some(rule_opts) = rule.options.formatting.clone()
      {
        options.update(rule_opts);
      }
    }
  }

  /// Iterate the key-scoped formatter overrides that apply to one file.
  ///
  /// Each item pairs one dotted-key glob from a matching rule's [`Rule::keys`] with that
  /// rule's formatter options. Rules without keys are whole-document overrides and belong to
  /// [`Config::update_format_options`] instead.
  pub fn format_scopes<'s>(&'s self, path: &'s Path) -> impl Iterator<Item = (&'s String, formatter::OptionsIncomplete)> + Clone + 's {
    self
      .rules_for(path)
      .filter_map(|rule| {
        rule
          .keys
          .as_ref()
          .zip(rule.options.formatting.as_ref())
          .map(|(keys, options)| keys.iter().map(move |key| (key, options.clone())))
      })
      .flatten()
  }

  /// Return whether schema validation is enabled for one file.
  ///
  /// Validation is enabled unless it is switched off, and switching off only narrows: a
  /// matching whole-document rule that sets `schema.enabled = false` disables validation, but
  /// no rule can re-enable it once the global options disabled it. Key-scoped rules do not
  /// affect whole-document validation.
  #[must_use]
  pub fn is_schema_enabled(&self, path: &Path) -> bool {
    let enabled = self
      .global_options
      .schema
      .as_ref()
      .and_then(|schema| schema.enabled)
      .unwrap_or(true);

    for rule in self.rules_for(path).filter(|rule| rule.keys.is_none()) {
      if rule.options.schema.as_ref().and_then(|schema| schema.enabled) == Some(false) {
        return false;
      }
    }

    enabled
  }

  /// Transform all relative glob patterns to have the given base path.
  fn make_absolute(&mut self, environment: &impl Environment, base: &Path) -> Result<(), ConfigError> {
    make_patterns_absolute(self.include.as_mut(), environment, base)?;
    make_patterns_absolute(self.exclude.as_mut(), environment, base)?;

    for rule in &mut self.rule {
      make_patterns_absolute(rule.include.as_mut(), environment, base)?;
      make_patterns_absolute(rule.exclude.as_mut(), environment, base)?;
    }
    Ok(())
  }
}

/// Resolve every relative pattern against one configuration base directory.
fn make_patterns_absolute(
  pending_patterns: Option<&mut Vec<String>>,
  environment: &impl Environment,
  base: &Path,
) -> Result<(), ConfigError> {
  let Some(patterns) = pending_patterns else {
    return Ok(());
  };
  for pattern in patterns {
    if !environment.is_absolute(Path::new(pattern))? {
      *pattern = path_pattern(base.join(pattern.as_str()).normalize())?;
    }
  }
  Ok(())
}

/// Convert one normalized host path into the configuration glob string model.
#[allow(
  clippy::single_call_fn,
  reason = "path conversion keeps host Unicode failure context out of the repeated include and exclude normalization loop"
)]
fn path_pattern(path: PathBuf) -> Result<String, ConfigError> {
  path
    .into_os_string()
    .into_string()
    .map_err(|rejected_path| ConfigError::PathUnicode {
      path: PathBuf::from(rejected_path),
    })
}

/// The option set a configuration or a single `[[rule]]` can carry.
///
/// Both fields are optional so that a rule states only what it overrides; unset options keep
/// the value already resolved by the global options or an earlier matching rule.
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Options {
  /// Schema validation options.
  pub schema:     Option<SchemaOptions>,
  /// Formatting options.
  pub formatting: Option<formatter::OptionsIncomplete>,
}

impl Options {
  /// Normalize schema locations against a configuration base path.
  fn prepare(&mut self, environment: &impl Environment, base: &Path) -> Result<(), ConfigError> {
    let Some(schema_options) = self.schema.as_mut() else {
      return Ok(());
    };
    schema_options.url = match schema_options.path.take() {
      Some(configured_path) => Some(schema_path_url(environment, base, configured_path)?),
      None => schema_options.url.take(),
    };
    Ok(())
  }
}

/// Resolve one configured schema path into a file URL.
#[allow(
  clippy::single_call_fn,
  reason = "schema path resolution isolates host absoluteness and file-URL validation from option mutation"
)]
fn schema_path_url(environment: &impl Environment, base: &Path, configured_path: String) -> Result<Url, ConfigError> {
  let path = if environment.is_absolute(Path::new(&configured_path))? {
    PathBuf::from(configured_path)
  } else {
    base.join(configured_path).normalize()
  };
  environment.to_file_url(&path)?.ok_or(ConfigError::InvalidSchemaPath {
    path,
  })
}

/// A rule to override options by either name or file.
#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
  /// The name of the rule.
  ///
  /// Used in `taplo::<name>` comments.
  pub name: Option<String>,

  /// Files this rule is valid for.
  ///
  /// A list of Unix-like [glob](https://en.wikipedia.org/wiki/Glob_(programming)) path patterns.
  ///
  /// Relative paths are **not** relative to the configuration file, but rather
  /// depends on the tool using the configuration.
  ///
  /// Omitting this property includes all files, **however an empty array will include none**.
  pub include: Option<Vec<String>>,

  /// Files that are excluded from this rule.
  ///
  /// A list of Unix-like [glob](https://en.wikipedia.org/wiki/Glob_(programming)) path patterns.
  ///
  /// Relative paths are **not** relative to the configuration file, but rather
  /// depends on the tool using the configuration.
  ///
  /// This has priority over `include`.
  pub exclude: Option<Vec<String>>,

  /// Keys the rule is valid for in a document.
  ///
  /// A list of Unix-like [glob](https://en.wikipedia.org/wiki/Glob_(programming)) dotted key patterns.
  ///
  /// This allows enabling the rule for specific paths in the document.
  ///
  /// For example:
  ///
  /// - `package.metadata` will enable the rule for everything inside the `package.metadata` table,
  ///   including itself.
  ///
  /// If omitted, the rule will always be valid for all keys.
  pub keys: Option<Vec<String>>,

  /// Options this rule overrides for the documents and keys it covers.
  #[serde(flatten)]
  pub options: Options,

  /// Compiled `include`/`exclude` matcher, populated by [`Rule::prepare`].
  ///
  /// [`Rule::is_included`] matches every path while this is `None`.
  #[serde(skip)]
  pub matcher: Option<GlobRule>,
}

impl Rule {
  /// Prepare this rule for matching.
  ///
  /// # Errors
  ///
  /// Returns [`ConfigError`] when a host path operation, glob expression, or
  /// schema path is invalid.
  pub fn prepare(&mut self, environment: &impl Environment, base: &Path) -> Result<(), ConfigError> {
    let default_include = String::from("**");
    let include = self.include.as_deref().unwrap_or_else(|| from_ref(&default_include));
    let exclude = self.exclude.as_deref().unwrap_or(&[]);
    self.matcher = Some(GlobRule::new(include, exclude)?);
    self.options.prepare(environment, base)?;
    Ok(())
  }

  /// Return whether this rule applies to one file.
  ///
  /// An unprepared rule has no compiled matcher and applies to every path.
  #[must_use]
  pub fn is_included(&self, path: &Path) -> bool {
    self.matcher.as_ref().is_none_or(|rule| rule.is_match(path))
  }
}

/// Options for schema validation and completion.
///
/// Schemas in rules with defined keys are ignored.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemaOptions {
  /// Whether the schema should be enabled or not.
  ///
  /// Defaults to true if omitted.
  pub enabled: Option<bool>,

  /// A local file path to the schema, overrides `url` if set.
  ///
  /// URLs are also accepted here, but it's not a guarantee and might
  /// change in newer releases.
  /// Please use the `url` field instead whenever possible.
  pub path: Option<String>,

  /// A full absolute URL to the schema.
  ///
  /// The url of the schema, supported schemes are `http`, `https`, `file` and `taplo`.
  pub url: Option<Url>,
}

/// A plugin to extend Taplo's capabilities.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plugin {
  /// Optional settings for the plugin.
  #[serde(default)]
  pub settings: Option<Value>,
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::path::Path;

  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;
  use taplo::formatter::Options as FormatOptions;
  use taplo::formatter::OptionsIncomplete;

  use super::Config;
  use super::ConfigError;
  use super::Options;
  use super::Rule;
  use super::SchemaOptions;
  use crate::test_support::TestEnvironment;
  fn formatting(column_width: usize) -> Options {
    Options {
      formatting: Some(OptionsIncomplete {
        column_width: Some(column_width),
        ..OptionsIncomplete::default()
      }),
      ..Options::default()
    }
  }

  fn schema(enabled: bool) -> Options {
    Options {
      schema:     Some(SchemaOptions {
        enabled: Some(enabled),
        path:    None,
        url:     None,
      }),
      formatting: None,
    }
  }

  /// Construct one file rule with optional key scoping.
  fn rule(pattern: &str, keys: Option<Vec<String>>, options: Options) -> Rule {
    Rule {
      include: Some(Vec::from([pattern.into()])),
      keys,
      options,
      ..Rule::default()
    }
  }

  /// A complete configuration and the result of preparing it.
  #[derive(Debug)]
  struct PreparedConfig {
    /// Configuration retaining any partial normalization on failure.
    config: Config,
    /// Native preparation outcome.
    result: Result<(), ConfigError>,
  }

  /// Construct and prepare one configuration fixture at the shared workspace root.
  fn prepared_config(global_options: Options, rule: Vec<Rule>) -> PreparedConfig {
    let mut config = Config {
      rule,
      global_options,
      ..Config::default()
    };
    let result = config.prepare(&TestEnvironment::default(), Path::new("/workspace"));
    PreparedConfig {
      config,
      result,
    }
  }

  /// Prepared rules and both complete formatter option sets.
  type FormattingObservations = (PreparedConfig, FormatOptions, FormatOptions);

  #[test]
  fn whole_document_formatting_uses_only_matching_file_rules() -> Result<(), Box<PredicateFailure<FormattingObservations>>> {
    let prepared = prepared_config(
      formatting(80),
      Vec::from([
        rule("**/match.toml", None, formatting(100)),
        rule("**/other.toml", None, formatting(120)),
        rule("**/match.toml", Some(Vec::from(["package.metadata".into()])), formatting(140)),
      ]),
    );
    let mut matching = FormatOptions::default();
    prepared
      .config
      .update_format_options(Path::new("/workspace/match.toml"), &mut matching);
    let mut unrelated = FormatOptions::default();
    prepared
      .config
      .update_format_options(Path::new("/workspace/unrelated.toml"), &mut unrelated);
    ensure_that(
      (prepared, matching, unrelated),
      "only matching whole-document rules must override global formatting",
      |actual| actual.0.result.is_ok() && actual.1.column_width == 100 && actual.2.column_width == 80,
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_disablement_is_matching_file_scoped_and_only_narrows() -> Result<(), impl Debug> {
    let enabled = prepared_config(
      schema(true),
      Vec::from([
        rule("**/disabled.toml", None, schema(false)),
        rule("**/key-scoped.toml", Some(Vec::from(["nested".into()])), schema(false)),
      ]),
    );
    let disabled = prepared_config(schema(false), Vec::from([rule("**/*.toml", None, schema(true))]));
    ensure_that(
      [enabled, disabled],
      "file rules must narrow schema enablement without affecting unrelated or key-scoped files",
      |actual| {
        let [ref enabled_config, ref disabled_config] = *actual;
        enabled_config.result.is_ok()
          && disabled_config.result.is_ok()
          && !enabled_config.config.is_schema_enabled(Path::new("/workspace/disabled.toml"))
          && enabled_config.config.is_schema_enabled(Path::new("/workspace/other.toml"))
          && enabled_config.config.is_schema_enabled(Path::new("/workspace/key-scoped.toml"))
          && !disabled_config.config.is_schema_enabled(Path::new("/workspace/file.toml"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// URL parsing and the configuration prepared from it.
  type SchemaPathObservations = (Result<url::Url, url::ParseError>, Option<PreparedConfig>);

  #[test]
  fn schema_paths_use_platform_file_url_conversion_and_override_urls() -> Result<(), Box<PredicateFailure<SchemaPathObservations>>> {
    let url = url::Url::parse("https://example.com/ignored.json");
    let prepared = url.as_ref().ok().map(|ignored| {
      prepared_config(
        Options {
          schema:     Some(SchemaOptions {
            enabled: None,
            path:    Some("schemas/project.json".into()),
            url:     Some(ignored.clone()),
          }),
          formatting: None,
        },
        Vec::new(),
      )
    });
    ensure_that(
      (url, prepared),
      "a schema path must become an absolute file URL and override the URL field",
      |actual| {
        actual.1.as_ref().is_some_and(|observed| {
          observed.result.is_ok()
            && observed
              .config
              .global_options
              .schema
              .as_ref()
              .and_then(|schema_options| schema_options.url.as_ref())
              .is_some_and(|schema_url| schema_url.as_str() == "file:///workspace/schemas/project.json")
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
