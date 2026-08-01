//! WebAssembly bindings for formatting, linting, conversion, CLI, and local LSP use.

#![forbid(unsafe_code)]

use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use js_sys::Function;
use js_sys::Promise;
use serde::Serialize;
use taplo::formatter;
use taplo::parser::Diagnostic as ParserDiagnostic;
use taplo::parser::parse;
use taplo_common::config::Config;
use taplo_common::convert;
use taplo_common::schema::Schemas;
use taplo_common::schema::transport::local_http_client;
use url::Url;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_futures::future_to_promise;

mod environment;
#[cfg(feature = "lsp")]
/// Local WebAssembly LSP binding and JavaScript output transport.
pub mod lsp;

/// A local WebAssembly future that may retain JavaScript current-thread state.
type LocalWasmFuture<'operation, Output> = Pin<Box<dyn Future<Output = Output> + 'operation>>;

/// Shared pending JavaScript callback operation state.
struct JsAsyncOperation {
  /// Active promise returned by the callback.
  future:   Option<JsFuture>,
  /// Validated callback.
  callback: Function,
}

/// Local async reader state backed by one validated JavaScript callback.
struct JsAsyncRead {
  /// Shared callback operation state.
  operation: JsAsyncOperation,
}

/// Local async writer state backed by one validated JavaScript callback.
struct JsAsyncWrite {
  /// Shared callback operation state.
  operation: JsAsyncOperation,
}

/// Convert one thrown JavaScript value into stable text without debug formatting.
fn js_error_message(javascript_error: &JsValue) -> String {
  if let Some(message) = javascript_error.as_string() {
    return message;
  }
  if let Some(error_object) = javascript_error.dyn_ref::<js_sys::Error>() {
    return String::from(error_object.message());
  }
  "non-string JavaScript exception".into()
}

/// Validated local JavaScript host callbacks.
#[derive(Clone)]
struct WasmEnvironment {
  /// Clock callback.
  now:              Function,
  /// Single environment-variable callback.
  env_var:          Function,
  /// Environment enumeration callback.
  env_vars:         Function,
  /// Standard-error terminal callback.
  atty_stderr:      Function,
  /// Standard-input callback.
  stdin:            Function,
  /// Standard-output callback.
  stdout:           Function,
  /// Standard-error callback.
  stderr:           Function,
  /// Filesystem glob callback.
  glob_files:       Function,
  /// File read callback.
  read_file:        Function,
  /// File write callback.
  write_file:       Function,
  /// URL-to-path callback.
  to_file_path:     Function,
  /// Path-to-URL callback.
  to_file_url:      Function,
  /// Absolute-path callback.
  is_absolute:      Function,
  /// Current-directory callback.
  cwd:              Function,
  /// Configuration search callback.
  find_config_file: Function,
}

/// Decode one JavaScript host and prepare one configuration against it.
fn prepared_environment(host: JsValue, configuration_value: JsValue) -> Result<(WasmEnvironment, Config), JsError> {
  let mut configuration = if configuration_value.is_undefined() {
    Config::default()
  } else {
    serde_wasm_bindgen::from_value(configuration_value).map_err(js_error)?
  };
  let environment = WasmEnvironment::try_from(host).map_err(js_error)?;
  configuration.prepare(&environment, Path::new("/")).map_err(js_error)?;
  Ok((environment, configuration))
}

/// Serialize one typed boundary failure as a JavaScript [`Error`](JsError).
fn js_error(error: impl Display) -> JsError {
  JsError::new(&error.to_string())
}

/// Serialize a Rust value into its JavaScript wire representation.
fn js_value(serializable: &impl Serialize) -> Result<JsValue, JsError> {
  serde_wasm_bindgen::to_value(serializable).map_err(js_error)
}

/// Byte range exposed by lint diagnostics.
#[derive(Serialize)]
struct Range {
  /// Inclusive start byte offset.
  start: u32,
  /// Exclusive end byte offset.
  end:   u32,
}

/// One syntax, semantic, or schema lint diagnostic.
#[derive(Serialize)]
struct LintError {
  /// Source range when the diagnostic has an exact syntax origin.
  #[serde(skip_serializing_if = "Option::is_none")]
  range:   Option<Range>,
  /// Human-readable diagnostic text.
  #[serde(rename = "error")]
  message: String,
}

/// Complete lint response.
#[derive(Serialize)]
struct LintResult {
  /// Ordered diagnostics produced by the requested lint run.
  errors: Vec<LintError>,
}

/// Serialize a complete lint response for JavaScript.
fn lint_result(diagnostics: Vec<LintError>) -> Result<JsValue, JsError> {
  js_value(&LintResult {
    errors: diagnostics
  })
}

/// Install the WebAssembly panic-reporting hook once.
#[wasm_bindgen]
pub fn initialize() {
  console_error_panic_hook::set_once();
}

/// Format one TOML document using JavaScript-supplied environment and configuration values.
///
/// # Errors
///
/// Returns [`JsError`] when environment validation, configuration decoding,
/// parsing, scoped formatting, or JavaScript serialization fails.
#[wasm_bindgen]
pub fn format(env: JsValue, toml: &str, options: JsValue, config: JsValue) -> Result<String, JsError> {
  let (environment, configuration) = prepared_environment(env, config)?;
  drop(environment);

  let camel_options: formatter::OptionsIncompleteCamel = serde_wasm_bindgen::from_value(options).map_err(js_error)?;
  let mut format_options = formatter::Options::default();
  if let Some(configuration_options) = configuration.global_options.formatting.clone() {
    format_options.update(configuration_options);
  }
  format_options.update_camel(camel_options);

  let syntax = parse(toml).map_err(js_error)?;
  let error_ranges = syntax.diagnostics().iter().map(ParserDiagnostic::range).collect::<Vec<_>>();

  formatter::format_with_path_scopes(
    &syntax.into_dom(),
    &format_options,
    &error_ranges,
    configuration.format_scopes(Path::new("")),
  )
  .map_err(js_error)
}

/// Lint one TOML document with syntax, semantic, and associated-schema diagnostics.
///
/// # Errors
///
/// The returned promise rejects with a [`JsError`] when environment
/// validation, configuration decoding, parsing, schema loading, schema
/// validation, or JavaScript serialization fails.
#[wasm_bindgen]
pub fn lint(env: JsValue, toml: String, config: JsValue) -> Promise {
  future_to_promise(async move { lint_local(env, toml, config).await.map_err(JsValue::from) })
}

/// Execute one lint operation inside the local WebAssembly capability boundary.
fn lint_local(host: JsValue, toml: String, configuration_value: JsValue) -> LocalWasmFuture<'static, Result<JsValue, JsError>> {
  Box::pin(async move {
    let (environment, configuration) = prepared_environment(host, configuration_value)?;

    let syntax = parse(&toml).map_err(js_error)?;

    if !syntax.diagnostics().is_empty() {
      return lint_result(
        syntax
          .diagnostics()
          .iter()
          .map(|diagnostic| LintError {
            range:   Range {
              start: diagnostic.range().start().into(),
              end:   diagnostic.range().end().into(),
            }
            .into(),
            message: diagnostic.to_string(),
          })
          .collect(),
      );
    }

    let dom = syntax.into_dom();

    if let Err(diagnostics) = dom.validate() {
      return lint_result(
        diagnostics
          .into_iter()
          .map(|diagnostic| LintError {
            range:   None,
            message: diagnostic.to_string(),
          })
          .collect(),
      );
    }

    let http = local_http_client().map_err(js_error)?;
    let schemas = Schemas::new_local(environment, http).map_err(js_error)?;
    schemas.associations().add_from_config(&configuration);

    let document_url = Url::parse("file:///__.toml").map_err(js_error)?;
    if let Some(schema) = schemas.associations().association_for(&document_url) {
      let schema_diagnostics = schemas.validate_root(&schema.url, &dom).await.map_err(js_error)?;

      return lint_result(
        schema_diagnostics
          .into_iter()
          .map(|diagnostic| LintError {
            range:   None,
            message: diagnostic.message,
          })
          .collect(),
      );
    }

    lint_result(Vec::new())
  })
}

/// Convert syntactically and semantically valid TOML into JSON text.
///
/// # Errors
///
/// Returns [`JsError`] when parsing, semantic validation, or JSON
/// serialization fails.
#[wasm_bindgen]
pub fn to_json(toml: &str) -> Result<String, JsError> {
  convert::toml_to_json_with_format(toml, convert::JsonFormatting::Compact).map_err(js_error)
}

/// Convert JSON text into TOML.
///
/// # Errors
///
/// Returns [`JsError`] when JSON decoding or TOML rendering fails.
#[wasm_bindgen]
pub fn from_json(json: &str) -> Result<String, JsError> {
  convert::json_to_toml(json, false).map_err(js_error)
}

/// Run the command-line interface against validated JavaScript host callbacks.
///
/// # Errors
///
/// The returned promise rejects with a [`JsError`] when host validation,
/// argument parsing, output, logging initialization, CLI construction, or
/// command execution fails.
#[cfg(feature = "cli")]
#[wasm_bindgen]
pub fn run_cli(env: JsValue, args: JsValue) -> Promise {
  future_to_promise(async move {
    match run_cli_local(env, args).await {
      Ok(()) => Ok(JsValue::undefined()),
      Err(error) => Err(JsValue::from(error)),
    }
  })
}

/// Execute the CLI through its explicit local-capability entry point.
#[cfg(feature = "cli")]
fn run_cli_local(host: JsValue, argument_value: JsValue) -> LocalWasmFuture<'static, Result<(), JsError>> {
  use clap::Parser as _;
  use taplo_cli::Taplo;
  use taplo_cli::args::Colors;
  use taplo_cli::args::TaploArgs;
  use taplo_common::log::setup_stderr_logging;
  use tracing::Instrument as _;

  Box::pin(async move {
    let environment = WasmEnvironment::try_from(host).map_err(js_error)?;
    let arguments: Vec<String> = serde_wasm_bindgen::from_value(argument_value).map_err(js_error)?;

    let cli_arguments = match TaploArgs::try_parse_from(arguments) {
      Ok(cli_arguments) => cli_arguments,
      Err(error) => return Err(js_error(error)),
    };

    setup_stderr_logging(
      &environment,
      cli_arguments.log_spans,
      cli_arguments.verbose,
      match cli_arguments.colors {
        Colors::Auto => None,
        Colors::Always => Some(true),
        Colors::Never => Some(false),
      },
    )
    .map_err(js_error)?;

    let mut taplo = Taplo::new(environment.clone()).map_err(js_error)?;
    taplo
      .execute_local(cli_arguments)
      .instrument(tracing::info_span!("taplo"))
      .await
      .map_err(js_error)
  })
}

/// Construct the local WebAssembly language server and validate all callbacks.
///
/// # Errors
///
/// Returns [`JsError`] when host callbacks, output callbacks, HTTP transport,
/// world state, or logging cannot initialize.
#[cfg(feature = "lsp")]
#[wasm_bindgen]
pub fn create_lsp(env: JsValue, lsp_interface: JsValue) -> Result<lsp::TaploWasmLsp, JsError> {
  use taplo_common::log::setup_stderr_logging;

  let environment = WasmEnvironment::try_from(env).map_err(js_error)?;
  let interface = lsp::WasmLspInterface::try_from(lsp_interface).map_err(js_error)?;
  let http = local_http_client().map_err(js_error)?;
  let world = taplo_lsp::create_local_world(environment.clone(), http).map_err(js_error)?;

  setup_stderr_logging(&environment, false, false, None).map_err(js_error)?;

  Ok(lsp::new(taplo_lsp::create_local_server(), world, interface))
}
