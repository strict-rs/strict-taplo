//! Installed Taplo command-line composition.

use std::fmt;
use std::future::Future;
use std::io::Error as IoError;
use std::num::TryFromIntError;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::Utf8Error;
use std::string::FromUtf8Error;
use std::sync::Arc;

use codespan_reporting::files::Error as DiagnosticError;
use glob::PatternError;
use serde_json::Error as JsonError;
use taplo::dom::QueryError;
use taplo::dom::RenderError;
use taplo::formatter::FormatError;
use taplo::formatter::OptionParseError;
use taplo::parser::ParseFailure;
use taplo_common::config::Config;
use taplo_common::config::ConfigError;
use taplo_common::environment::EnvironmentError;
use taplo_common::environment::LocalEnvironment;
#[cfg(feature = "lint")]
use taplo_common::schema::SchemaError;
#[cfg(feature = "lint")]
use taplo_common::schema::Schemas;
#[cfg(feature = "lint")]
use taplo_common::schema::associations::AssociationError;
#[cfg(feature = "lint")]
use taplo_common::schema::transport::LocalSchemaTransport;
#[cfg(feature = "lint")]
use taplo_common::schema::transport::TransportError;
#[cfg(feature = "lsp")]
use taplo_lsp::world::WorldError;
#[cfg(feature = "lsp")]
use taplo_lsp_async::ServerError;
use thiserror::Error;
use toml::de::Error as TomlDecodeError;
use toml::ser::Error as TomlEncodeError;
use url::ParseError as UrlParseError;

/// Command-line argument and subcommand DTOs.
pub mod args;
/// Command implementations and feature-aware dispatch.
pub mod commands;
/// Typed conversion and rendering of user-facing diagnostics.
pub mod printing;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

/// Test-target environment adapter around the shared deterministic host.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct TestEnvironment(taplo_test_support::TestEnvironment);

/// A CLI orchestration future that may retain browser or current-thread host state.
///
/// Native language-server execution creates its concurrent server internally;
/// the outer command future remains local because callers await it directly.
pub type LocalCommandFuture<'operation, Output> = Pin<Box<dyn Future<Output = Output> + 'operation>>;

/// Stable command failure categories selected at the CLI boundary.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CliFailure {
  /// A command requires a current working directory.
  #[error("the current working directory is required")]
  WorkingDirectoryRequired,
  /// A command requires host capabilities unavailable to a local-only executor.
  #[cfg(feature = "lsp")]
  #[error("the selected command requires a concurrent environment")]
  ConcurrentEnvironmentRequired,
  /// A document contains recoverable syntax diagnostics.
  #[error("the TOML document contains syntax errors")]
  SyntaxErrors,
  /// A document contains semantic diagnostics.
  #[error("the TOML document contains semantic errors")]
  SemanticErrors,
  /// Schema validation produced diagnostics.
  #[error("schema validation failed")]
  SchemaValidation,
  /// Formatting was withheld because syntax diagnostics were present.
  #[error("formatting was not performed because syntax errors were present")]
  FormattingBlocked,
  /// Check mode found an unformatted input.
  #[error("the input is not properly formatted")]
  FormattingMismatch,
  /// One or more files could not be formatted.
  #[error("one or more files could not be formatted")]
  FileFormattingFailed,
  /// One or more files failed validation.
  #[error("one or more files are invalid")]
  FileValidationFailed,
  /// A query matched no values.
  #[error("the query matched no values")]
  NoQueryMatches,
  /// A separator was supplied for an incompatible output format.
  #[error("`--separator` is only valid for `--output-format value`")]
  InvalidSeparator,
  /// Table values cannot be represented by scalar-value output.
  #[error("table values require `json` or `toml` output")]
  TableValueOutput,
  /// TOML test input is invalid.
  #[error("the TOML test input is invalid")]
  InvalidTomlTestInput,
}

/// Typed error returned by reusable CLI composition.
#[derive(Debug, Error)]
pub enum CliError {
  /// A stable command-level failure occurred.
  #[error(transparent)]
  Failure(#[from] CliFailure),
  /// A host capability failed.
  #[error(transparent)]
  Environment(#[from] EnvironmentError),
  /// Configuration preparation failed.
  #[error(transparent)]
  Config(#[from] ConfigError),
  /// Configuration TOML could not be decoded.
  #[error("configuration file `{path}` is invalid")]
  ConfigDecode {
    /// Invalid configuration path.
    path:   PathBuf,
    /// Underlying TOML decoder failure.
    #[source]
    source: TomlDecodeError,
  },
  /// TOML serialization failed.
  #[error("failed to serialize TOML output")]
  TomlEncode(#[from] TomlEncodeError),
  /// JSON serialization failed.
  #[error("failed to encode or decode JSON")]
  Json(#[from] JsonError),
  /// A file was not UTF-8.
  #[error("input is not valid UTF-8")]
  Utf8(#[from] Utf8Error),
  /// An owned byte buffer was not UTF-8.
  #[error("input is not valid UTF-8")]
  OwnedUtf8(#[from] FromUtf8Error),
  /// An asynchronous stream operation failed.
  #[error("command I/O failed")]
  Io(#[from] IoError),
  /// A parser tree could not be constructed.
  #[error(transparent)]
  Parse(#[from] ParseFailure),
  /// Source formatting failed.
  #[error(transparent)]
  Format(#[from] FormatError),
  /// A formatter option was invalid.
  #[error(transparent)]
  FormatOption(#[from] OptionParseError),
  /// TOML rendering failed.
  #[error(transparent)]
  Render(#[from] RenderError),
  /// A DOM query was invalid.
  #[error(transparent)]
  Query(#[from] QueryError),
  /// A CLI file pattern was invalid.
  #[error("invalid file glob `{pattern}`")]
  Glob {
    /// Rejected expression.
    pattern: String,
    /// Underlying glob parser failure.
    #[source]
    source:  PatternError,
  },
  /// A URL constructed at a command boundary was invalid.
  #[error("invalid URL `{input}`")]
  Url {
    /// Rejected URL source.
    input:  String,
    /// Underlying URL parser failure.
    #[source]
    source: UrlParseError,
  },
  /// A host file path could not be represented as a file URL.
  #[error("file path `{path}` cannot be represented as a URL")]
  FileUrl {
    /// Rejected file path.
    path: PathBuf,
  },
  /// A file path cannot be represented by the CLI's string glob model.
  #[error("file path `{path}` is not valid Unicode")]
  PathUnicode {
    /// Rejected file path.
    path: PathBuf,
  },
  /// A schema association failed.
  #[cfg(feature = "lint")]
  #[error(transparent)]
  Association(#[from] AssociationError),
  /// Schema loading or validation failed.
  #[cfg(feature = "lint")]
  #[error(transparent)]
  Schema(#[from] SchemaError),
  /// Schema transport construction failed.
  #[cfg(feature = "lint")]
  #[error(transparent)]
  Transport(#[from] TransportError),
  /// Diagnostic rendering failed.
  #[error("failed to render diagnostics")]
  Diagnostic(#[from] DiagnosticError),
  /// A Rowan source coordinate does not fit the host diagnostic index type.
  #[error("source coordinate `{offset}` exceeds the host diagnostic range")]
  CoordinateOverflow {
    /// Rejected byte offset.
    offset: u32,
    /// Underlying host-index conversion failure.
    #[source]
    source: TryFromIntError,
  },
  /// Shell completion generation received an unsupported shell name.
  #[error("unsupported completion shell `{shell}`: {message}")]
  InvalidShell {
    /// Rejected shell name.
    shell:   String,
    /// Parser diagnostic.
    message: String,
  },
  /// Operating-system shutdown signal registration failed.
  #[cfg(feature = "lsp")]
  #[error("failed to register shutdown signal: {message}")]
  ShutdownSignal {
    /// Signal registration diagnostic.
    message: String,
  },
  /// Language-server world construction failed.
  #[cfg(feature = "lsp")]
  #[error(transparent)]
  LspWorld(Box<WorldError>),
  /// Language-server transport or protocol execution failed.
  #[cfg(feature = "lsp")]
  #[error(transparent)]
  LspServer(#[from] ServerError),
}

#[cfg(feature = "lsp")]
impl From<WorldError> for CliError {
  fn from(error: WorldError) -> Self {
    Self::LspWorld(Box::new(error))
  }
}

/// CLI state using local asynchronous host operations.
pub struct Taplo<E: LocalEnvironment> {
  /// Host capabilities.
  env:     E,
  /// Whether diagnostic output uses ANSI styling.
  colors:  bool,
  /// Schema services when linting is enabled.
  #[cfg(feature = "lint")]
  schemas: Schemas<LocalSchemaTransport<E>>,
  /// Prepared configuration cached across one invocation.
  config:  Option<Arc<Config>>,
}

impl<E: LocalEnvironment> fmt::Debug for Taplo<E> {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("Taplo")
      .field("colors", &self.colors)
      .field("config_loaded", &self.config.is_some())
      .finish_non_exhaustive()
  }
}

/// Borrow one host path through the CLI's Unicode display and glob model.
pub(crate) fn path_text(path: &Path) -> Result<&str, CliError> {
  path.to_str().ok_or_else(|| CliError::PathUnicode {
    path: path.to_owned()
  })
}

/// Return the command's built-in default configuration.
#[must_use]
pub fn default_config() -> Config {
  Config {
    plugins: None,
    ..Default::default()
  }
}
