#[cfg(feature = "completions")]
use std::io::stdout;
use std::path::Path;
use std::path::PathBuf;
use std::str;
#[cfg(feature = "completions")]
use std::str::FromStr as _;
use std::sync::Arc;
#[cfg(all(not(target_arch = "wasm32"), feature = "lint"))]
use std::time::Duration;

#[cfg(feature = "completions")]
use clap::CommandFactory as _;
#[cfg(feature = "completions")]
use clap_complete::generate;
#[cfg(feature = "completions")]
use clap_complete::shells::Shell;
use itertools::Itertools as _;
use taplo_common::config::Config;
#[cfg(feature = "lsp")]
use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::LocalEnvironment;
#[cfg(feature = "lint")]
use taplo_common::schema::Schemas;
#[cfg(all(not(target_arch = "wasm32"), feature = "lint"))]
use taplo_common::schema::transport::concurrent_http_client;
#[cfg(all(target_arch = "wasm32", feature = "lint"))]
use taplo_common::schema::transport::local_http_client;
use taplo_common::util::Normalize as _;

use crate::CliError;
use crate::CliFailure;
use crate::LocalCommandFuture;
use crate::Taplo;
use crate::args::Colors;
use crate::args::ConfigCommand;
use crate::args::FormatCommand;
use crate::args::GeneralArgs;
use crate::args::GetCommand;
#[cfg(feature = "lint")]
use crate::args::LintCommand;
#[cfg(feature = "lsp")]
use crate::args::LspCommand;
use crate::args::TaploArgs;
use crate::args::TaploCommand;

/// Default-configuration and configuration-schema output.
mod config;
/// TOML formatting command behavior.
mod format;
/// TOML syntax, semantic, and schema linting.
#[cfg(feature = "lint")]
mod lint;
/// Native concurrent language-server execution.
#[cfg(feature = "lsp")]
mod lsp;
/// Semantic TOML query and output behavior.
mod queries;

/// TOML conformance decoder behavior.
#[cfg(feature = "toml-test")]
mod toml_test;

/// Generate the single feature-aware command selection table.
macro_rules! dispatch_command {
  ($taplo:ident, $command:expr,lsp($lsp_command:ident) => $lsp_result:expr) => {
    match $command {
      #[cfg(feature = "completions")]
      TaploCommand::Completions {
        shell,
      } => {
        let parsed_shell = match Shell::from_str(&shell) {
          Ok(parsed_shell) => parsed_shell,
          Err(error) => {
            return Err(CliError::InvalidShell {
              shell,
              message: error,
            });
          }
        };
        let mut completion_command = TaploArgs::command();
        let binary_name = completion_command.get_bin_name().unwrap_or("taplo").to_owned();
        let mut output = stdout();
        generate(parsed_shell, &mut completion_command, binary_name, &mut output);
        Ok(())
      }
      TaploCommand::Config {
        cmd: command,
      } => config::execute_config($taplo, command).await,
      TaploCommand::Format(command) => format::execute_format($taplo, command).await,
      TaploCommand::Get(command) => queries::execute_get($taplo, command).await,
      #[cfg(feature = "lint")]
      TaploCommand::Lint(command) => lint::execute_lint($taplo, command).await,
      #[cfg(feature = "lsp")]
      TaploCommand::Lsp {
        cmd: $lsp_command,
      } => $lsp_result,
      #[cfg(feature = "toml-test")]
      TaploCommand::TomlTest {} => toml_test::execute_toml_test($taplo).await,
    }
  };
}

impl<E: LocalEnvironment> Taplo<E> {
  /// Construct command composition around one host environment.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when terminal detection or schema transport
  /// initialization fails.
  pub fn new(env: E) -> Result<Self, CliError> {
    #[cfg(all(not(target_arch = "wasm32"), feature = "lint"))]
    let http = concurrent_http_client(&env, Duration::from_secs(5))?;

    #[cfg(all(target_arch = "wasm32", feature = "lint"))]
    let http = local_http_client()?;

    Ok(Self {
      #[cfg(feature = "lint")]
      schemas: Schemas::new_local(env.clone(), http)?,
      colors: env.atty_stderr()?,
      config: None,
      env,
    })
  }

  /// Execute a command against local asynchronous host capabilities.
  ///
  /// The language-server command is rejected with
  /// `CliFailure::ConcurrentEnvironmentRequired` when the `lsp` feature is
  /// enabled; browser callers use the dedicated local LSP API instead.
  ///
  /// # Errors
  ///
  /// Returns a typed command, host, parsing, schema, formatting, query,
  /// transport, or unavailable-capability failure from the selected operation.
  pub fn execute_local(&mut self, arguments: TaploArgs) -> LocalCommandFuture<'_, Result<(), CliError>> {
    Box::pin(async move {
      self.configure_colors(arguments.colors)?;
      dispatch_command!(
        self,
        arguments.cmd,
        lsp(_lsp_command) => Err(CliFailure::ConcurrentEnvironmentRequired.into())
      )
    })
  }

  /// Execute the selected command against this local-capability environment.
  ///
  /// # Errors
  ///
  /// Returns a typed command, host, parsing, schema, formatting, query, or
  /// transport failure from the selected operation.
  #[cfg(not(feature = "lsp"))]
  pub fn execute(&mut self, arguments: TaploArgs) -> LocalCommandFuture<'_, Result<(), CliError>> {
    self.execute_local(arguments)
  }

  /// Execute the selected command, using concurrent capabilities only for the
  /// native language-server command.
  ///
  /// # Errors
  ///
  /// Returns a typed command, host, parsing, schema, formatting, query,
  /// transport, or language-server failure from the selected operation.
  #[cfg(feature = "lsp")]
  pub fn execute(&mut self, arguments: TaploArgs) -> LocalCommandFuture<'_, Result<(), CliError>>
  where
    E: ConcurrentEnvironment,
    E::Stdin: Send,
    E::Stdout: Send,
  {
    Box::pin(async move {
      self.configure_colors(arguments.colors)?;
      dispatch_command!(
        self,
        arguments.cmd,
        lsp(lsp_command) => self.execute_lsp(lsp_command).await
      )
    })
  }

  /// Print the default configuration or its JSON schema.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when serialization or host output fails.
  pub fn execute_config(&self, command: ConfigCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
    config::execute_config(self, command)
  }

  /// Format standard input or selected TOML files.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when configuration, parsing, formatting, file
  /// selection, host I/O, or check-mode validation fails.
  pub fn execute_format(&mut self, command: FormatCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
    format::execute_format(self, command)
  }

  /// Query a TOML document and write the selected output representation.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when input, parsing, semantic validation, querying,
  /// rendering, serialization, or host output fails.
  pub fn execute_get(&self, command: GetCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
    queries::execute_get(self, command)
  }

  /// Lint standard input or selected TOML files.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when configuration, syntax, semantic, schema,
  /// transport, file selection, or host I/O fails.
  #[cfg(feature = "lint")]
  pub fn execute_lint(&mut self, command: LintCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
    lint::execute_lint(self, command)
  }

  /// Decode TOML-test input and emit its canonical JSON representation.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when input, parsing, semantic validation,
  /// serialization, or host output fails.
  #[cfg(feature = "toml-test")]
  pub fn execute_toml_test(&self) -> LocalCommandFuture<'_, Result<(), CliError>> {
    toml_test::execute_toml_test(self)
  }

  /// Run the native concurrent language server.
  ///
  /// # Errors
  ///
  /// Returns [`CliError`] when configuration, transport, world construction,
  /// shutdown registration, or server execution fails.
  #[cfg(feature = "lsp")]
  pub fn execute_lsp(&mut self, command: LspCommand) -> LocalCommandFuture<'_, Result<(), CliError>>
  where
    E: ConcurrentEnvironment,
    E::Stdin: Send,
    E::Stdout: Send,
  {
    lsp::execute_lsp(self, command)
  }

  /// Resolve and install the invocation's terminal color policy.
  fn configure_colors(&mut self, colors: Colors) -> Result<(), CliError> {
    self.colors = match colors {
      Colors::Auto => self.env.atty_stderr()?,
      Colors::Always => true,
      Colors::Never => false,
    };
    Ok(())
  }

  /// Load and prepare command configuration once.
  #[tracing::instrument(skip_all)]
  fn load_config<'operation>(
    &'operation mut self,
    general: &'operation GeneralArgs,
  ) -> LocalCommandFuture<'operation, Result<Arc<Config>, CliError>> {
    Box::pin(async move {
      if let Some(cached_config) = self.config.as_ref() {
        return Ok(Arc::clone(cached_config));
      }

      let mut config_path = general.config.clone();
      if config_path.is_none()
        && !general.no_auto_config
        && let Some(current_directory) = self.env.cwd_normalized()?
      {
        config_path = self.env.find_config_file_normalized(&current_directory).await?;
      }

      let mut prepared_config = if let Some(path) = config_path {
        tracing::info!(?path, "found configuration file");
        let bytes = self.env.read_file(&path).await?;
        let source_text = str::from_utf8(&bytes)?;
        toml::from_str(source_text).map_err(|decode_error| CliError::ConfigDecode {
          path,
          source: decode_error,
        })?
      } else {
        Config::default()
      };

      let base = self.env.cwd_normalized()?.ok_or(CliFailure::WorkingDirectoryRequired)?;
      prepared_config.prepare(&self.env, &base)?;

      let shared_config = Arc::new(prepared_config);
      self.config = Some(Arc::clone(&shared_config));
      Ok(shared_config)
    })
  }

  /// Collect unique normalized files selected by CLI and configuration globs.
  #[tracing::instrument(skip_all, fields(?cwd))]
  fn collect_files<'operation>(
    &'operation self,
    cwd: &'operation Path,
    config: &'operation Config,
    arg_patterns: impl Iterator<Item = String> + 'operation,
  ) -> LocalCommandFuture<'operation, Result<Vec<PathBuf>, CliError>> {
    Box::pin(async move {
      let mut patterns = Vec::new();
      for pattern in arg_patterns {
        if self.env.is_absolute(Path::new(&pattern))? {
          patterns.push(pattern);
        } else {
          patterns.push(path_pattern(&cwd.join(pattern).normalize())?);
        }
      }

      if patterns.is_empty() {
        patterns = config.include.clone().map_or_else(
          || path_pattern(&cwd.join("**/*.toml").normalize()).map(|pattern| Vec::from([pattern])),
          Ok,
        )?;
      }

      let mut files = Vec::new();
      for pattern in patterns.into_iter().unique() {
        let validated_pattern = glob::Pattern::new(&pattern).map_err(|source| CliError::Glob {
          pattern: pattern.clone(),
          source,
        })?;
        drop(validated_pattern);
        files.extend(self.env.glob_files_normalized(&pattern)?);
      }

      let total = files.len();
      files.retain(|path| config.is_included(path));
      let excluded = total.saturating_sub(files.len());
      tracing::info!(total, excluded, "found files");
      tracing::debug!(?files, "file details");
      Ok(files)
    })
  }
}

/// Convert one host path into the CLI glob string model without lossy replacement.
fn path_pattern(path: &Path) -> Result<String, CliError> {
  crate::path_text(path).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
  use std::path::Path;
  use std::path::PathBuf;
  use std::sync::Arc;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_lacks;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_common::config::Config;
  #[cfg(feature = "lsp")]
  use taplo_lsp_async::rpc;
  use taplo_test_support::drive;

  use crate::CliError;
  use crate::CliFailure;
  use crate::Taplo;
  use crate::TestEnvironment;
  use crate::args::Colors;
  use crate::args::ConfigCommand;
  use crate::args::FormatCommand;
  use crate::args::FormatInputPolicy;
  use crate::args::FormatOutputPolicy;
  use crate::args::GeneralArgs;
  use crate::args::GetCommand;
  #[cfg(feature = "lint")]
  use crate::args::LintCommand;
  use crate::args::OutputFormat;
  use crate::args::TaploArgs;
  use crate::args::TaploCommand;

  /// Construct shared command configuration with explicit discovery policy.
  fn config_general(config: Option<&str>, no_auto_config: bool) -> GeneralArgs {
    GeneralArgs {
      config: config.map(PathBuf::from),
      cache_path: None,
      no_auto_config,
    }
  }

  /// Construct shared command configuration that never searches ambient paths.
  fn general() -> GeneralArgs {
    config_general(None, true)
  }

  /// Construct one deterministic top-level CLI argument value.
  fn arguments(command: TaploCommand) -> TaploArgs {
    TaploArgs {
      colors:    Colors::Never,
      verbose:   false,
      log_spans: false,
      cmd:       command,
    }
  }

  /// Construct one CLI formatter command with default policies.
  fn format_command(files: Vec<String>) -> FormatCommand {
    FormatCommand {
      general: general(),
      options: Vec::new(),
      input: FormatInputPolicy {
        force: false
      },
      output: FormatOutputPolicy {
        check: false,
        diff:  false,
      },
      files,
      stdin_filepath: None,
    }
  }

  /// Install one standard-input fixture and construct its formatter command.
  fn stdin_format_command(environment: &TestEnvironment, source: &[u8]) -> FormatCommand {
    environment.set_stdin(source.to_vec());
    format_command(Vec::from([String::from("-")]))
  }

  /// Construct one value-oriented query command over standard input.
  fn get_command(pattern: Option<&str>) -> GetCommand {
    GetCommand {
      output_format: OutputFormat::Value,
      strip_newline: false,
      file_path:     None,
      pattern:       pattern.map(ToOwned::to_owned),
      separator:     None,
    }
  }

  /// Construct one query command with an explicit structured output format.
  fn formatted_get_command(pattern: Option<&str>, output_format: OutputFormat) -> GetCommand {
    GetCommand {
      output_format,
      ..get_command(pattern)
    }
  }

  /// Construct one schema-disabled lint command over standard input.
  #[cfg(feature = "lint")]
  fn lint_command() -> LintCommand {
    LintCommand {
      general:                 general(),
      schema:                  None,
      schema_catalog:          Vec::new(),
      default_schema_catalogs: false,
      no_schema:               true,
      files:                   Vec::from([String::from("-")]),
    }
  }

  /// Construct one bounded standard-I/O language-server command.
  #[cfg(feature = "lsp")]
  fn stdio_lsp_command() -> crate::args::LspCommand {
    crate::args::LspCommand {
      general: general(),
      io:      crate::args::LspCommandIo::Stdio {},
    }
  }

  /// Write one complete framed JSON-RPC value to the interactive client pipe.
  ///
  /// # Errors
  ///
  /// Returns a test failure when serialization or interactive input fails.
  #[cfg(feature = "lsp")]
  async fn send_lsp_message(
    writer: &mut taplo_test_support::TestInputWriter,
    message: &impl serde::Serialize,
    context: &'static str,
  ) -> Result<(), TestFailure> {
    let body = ensure_ok(serde_json::to_vec(message), "the bounded LSP client message must serialize")?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    ensure_ok(tokio::io::AsyncWriteExt::write_all(writer, &frame).await, context)
  }

  /// Interactive framed client for one bounded CLI language-server session.
  #[cfg(feature = "lsp")]
  struct BoundedLspClient {
    /// Fixture-owned standard-input producer.
    input:    taplo_test_support::TestInputWriter,
    /// Streaming reader over captured standard output.
    output:   tokio::io::BufReader<taplo_test_support::TestOutputReader>,
    /// Every response and asynchronous message consumed from output.
    observed: Vec<rpc::Message>,
  }

  #[cfg(feature = "lsp")]
  impl BoundedLspClient {
    /// Send one typed JSON-RPC request without awaiting its response.
    ///
    /// # Errors
    ///
    /// Returns a test failure when serialization or interactive input fails.
    async fn send_request(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      context: &'static str,
    ) -> Result<(), TestFailure> {
      let request = rpc::Request::new()
        .with_method(method)
        .with_id(Some(rpc::RequestId::Number(id)))
        .with_params(params);
      send_lsp_message(&mut self.input, &request, context).await
    }

    /// Send one typed JSON-RPC notification.
    ///
    /// # Errors
    ///
    /// Returns a test failure when serialization or interactive input fails.
    async fn notify(&mut self, method: &str, params: Option<serde_json::Value>, context: &'static str) -> Result<(), TestFailure> {
      let notification = rpc::Request::<serde_json::Value>::new().with_method(method).with_params(params);
      send_lsp_message(&mut self.input, &notification, context).await
    }

    /// Read framed output until one correlated response arrives.
    ///
    /// Every intervening notification or server request is retained for later
    /// asynchronous-effect assertions.
    ///
    /// # Errors
    ///
    /// Returns a test failure for invalid framing or malformed JSON-RPC output.
    async fn response(&mut self, request_id: i32) -> Result<rpc::Message, TestFailure> {
      loop {
        let mut header = String::new();
        let header_bytes = ensure_ok(
          tokio::io::AsyncBufReadExt::read_line(&mut self.output, &mut header).await,
          "the bounded LSP client must read a response header",
        )?;
        ensure(header_bytes > 0, "the server must not close output before the correlated response")?;
        let encoded_length = ensure_some(
          header
            .strip_prefix("Content-Length: ")
            .and_then(|value| value.strip_suffix("\r\n")),
          "server output must use the standard content-length header",
        )?;
        let content_length = ensure_ok(
          encoded_length.parse::<usize>(),
          "the server content length must be a decimal byte count",
        )?;

        let mut separator = String::new();
        let separator_bytes = ensure_ok(
          tokio::io::AsyncBufReadExt::read_line(&mut self.output, &mut separator).await,
          "the bounded LSP client must read the header separator",
        )?;
        ensure_eq(&separator_bytes, &2, "the server header separator must contain exactly CRLF")?;
        ensure_eq(
          &separator.as_str(),
          &"\r\n",
          "the server header must terminate before its JSON body",
        )?;

        let mut body = vec![0_u8; content_length];
        let body_bytes = ensure_ok(
          tokio::io::AsyncReadExt::read_exact(&mut self.output, &mut body).await,
          "the bounded LSP client must read the complete response body",
        )?;
        ensure_eq(
          &body_bytes,
          &content_length,
          "the framed response body must match its declared content length",
        )?;
        let message = ensure_ok(rpc::decode_slice(&body), "the server response body must decode as JSON-RPC")?;
        let correlated = message.id == rpc::MessageId::Value(rpc::RequestId::Number(request_id));
        if correlated {
          self.observed.push(message.clone());
          return Ok(message);
        }
        self.observed.push(message);
      }
    }

    /// Send one request and extract its successful correlated result.
    ///
    /// # Errors
    ///
    /// Returns a test failure when transport or decoding fails, the server
    /// emits an RPC error, or the response omits its result channel.
    async fn request(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      context: &'static str,
    ) -> Result<serde_json::Value, TestFailure> {
      self.send_request(id, method, params, context).await?;
      let response = self.response(id).await?;
      if let Some(error) = response.error {
        let cause = error
          .details
          .as_ref()
          .map_or_else(|| error.to_string(), |details| format!("{error}; details: {details}"));
        return Err(TestFailure::WasErr {
          context,
          cause,
        });
      }
      ensure_some(response.result, context)
    }

    /// Send one request and extract the JSON value at `pointer`.
    ///
    /// # Errors
    ///
    /// Returns a test failure from request transport, response decoding, or
    /// JSON pointer selection.
    async fn request_selected(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      pointer: &str,
      context: &'static str,
    ) -> Result<serde_json::Value, TestFailure> {
      let result = self.request(id, method, params, context).await?;
      ensure_some(result.pointer(pointer).cloned(), context)
    }

    /// Require one selected request value to satisfy its JSON contract.
    ///
    /// # Errors
    ///
    /// Returns a test failure from request transport, response decoding, JSON
    /// pointer selection, or the supplied value contract.
    async fn request_selected_where(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      pointer: &str,
      context: &'static str,
      validate: fn(&serde_json::Value, &'static str) -> Result<(), TestFailure>,
    ) -> Result<(), TestFailure> {
      let selected = self.request_selected(id, method, params, pointer, context).await?;
      validate(&selected, context)
    }

    /// Require one request result's text field to contain expected content.
    ///
    /// # Errors
    ///
    /// Returns a test failure from request transport, response decoding, text
    /// projection, or content comparison.
    async fn request_text_containing(
      &mut self,
      id: i32,
      method: &str,
      source_text: &str,
      expected_text: &str,
      context: &'static str,
    ) -> Result<(), TestFailure> {
      let converted = self
        .request_selected(
          id,
          method,
          Some(serde_json::json!({
            "text": source_text
          })),
          "/text",
          context,
        )
        .await?;
      let converted_text = ensure_some(converted.as_str(), context)?;
      ensure_contains(converted_text, expected_text, context)
    }
  }

  /// Extract one required JSON array.
  ///
  /// # Errors
  ///
  /// Returns a test failure when `value` is not an array.
  #[cfg(feature = "lsp")]
  fn json_array<'value>(value: &'value serde_json::Value, context: &'static str) -> Result<&'value [serde_json::Value], TestFailure> {
    ensure_some(value.as_array().map(Vec::as_slice), context)
  }

  /// Require one JSON value to be an array.
  ///
  /// # Errors
  ///
  /// Returns a test failure when `value` is not an array.
  #[cfg(feature = "lsp")]
  fn json_array_contract(value: &serde_json::Value, context: &'static str) -> Result<(), TestFailure> {
    let _values = json_array(value, context)?;
    Ok(())
  }

  /// Require one JSON value to be a non-empty array.
  ///
  /// # Errors
  ///
  /// Returns a test failure when `value` is not an array or contains no items.
  #[cfg(feature = "lsp")]
  fn non_empty_json_array_contract(value: &serde_json::Value, context: &'static str) -> Result<(), TestFailure> {
    let values = json_array(value, context)?;
    ensure(!values.is_empty(), context)
  }

  /// Require one JSON value to be protocol `null`.
  ///
  /// # Errors
  ///
  /// Returns a test failure when `value` is not `null`.
  #[cfg(feature = "lsp")]
  fn json_null_contract(value: &serde_json::Value, context: &'static str) -> Result<(), TestFailure> {
    ensure(value.is_null(), context)
  }

  /// Construct initialized CLI state and retain the observable host handle.
  fn initialized_cli() -> Result<(TestEnvironment, Taplo<TestEnvironment>), TestFailure> {
    let environment = TestEnvironment::default();
    let taplo = ensure_ok(Taplo::new(environment.clone()), "deterministic CLI state must initialize")?;
    Ok((environment, taplo))
  }

  /// Decode captured standard output as UTF-8.
  fn stdout_text(environment: &TestEnvironment) -> Result<String, TestFailure> {
    ensure_ok(String::from_utf8(environment.stdout()), "captured standard output must be UTF-8")
  }

  /// Execute one standard-input query from a clean output state and return its text.
  fn get_output(
    environment: &TestEnvironment,
    taplo: &Taplo<TestEnvironment>,
    source: &[u8],
    command: GetCommand,
    context: &'static str,
  ) -> Result<String, TestFailure> {
    environment.clear_output();
    environment.set_stdin(source.to_vec());
    ensure_ok(drive(taplo.execute_get(command)), context)?;
    stdout_text(environment)
  }

  /// Execute one text-producing query and compare its complete standard output.
  fn ensure_get_output(
    environment: &TestEnvironment,
    taplo: &Taplo<TestEnvironment>,
    source: &[u8],
    command: GetCommand,
    expected: &str,
    execution_context: &'static str,
    output_context: &'static str,
  ) -> Result<(), TestFailure> {
    let output = get_output(environment, taplo, source, command, execution_context)?;
    ensure_eq(&output.as_str(), &expected, output_context)
  }

  /// Install the shared schema requiring one string-valued `name` property.
  #[cfg(feature = "lint")]
  fn install_required_string_schema(environment: &TestEnvironment) {
    environment.insert_file(
      "/workspace/schema.json",
      br#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#.to_vec(),
    );
  }

  /// Decode captured standard error as UTF-8.
  fn stderr_text(environment: &TestEnvironment) -> Result<String, TestFailure> {
    ensure_ok(String::from_utf8(environment.stderr()), "captured standard error must be UTF-8")
  }

  /// Execute one formatter command and require its typed failure.
  fn format_failure(taplo: &mut Taplo<TestEnvironment>, command: FormatCommand, context: &'static str) -> Result<CliError, TestFailure> {
    ensure_some(drive(taplo.execute_format(command)).err(), context)
  }

  #[test]
  fn local_dispatcher_executes_local_commands() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    ensure_ok(
      drive(taplo.execute_local(arguments(TaploCommand::Config {
        cmd: ConfigCommand::Default,
      }))),
      "the local dispatcher must execute a local command",
    )?;
    let output = stdout_text(&environment)?;
    let decoded = ensure_ok(
      toml::from_str::<Config>(&output),
      "default configuration output must decode as the public configuration type",
    )?;
    ensure(decoded.include.is_none(), "default output must retain implicit inclusion")?;
    ensure(decoded.exclude.is_none(), "default output must retain implicit exclusion")?;
    ensure(decoded.rule.is_empty(), "default output must contain no path-specific rules")?;
    ensure(decoded.plugins.is_none(), "CLI defaults must omit plugin configuration")
  }

  #[test]
  fn configuration_commands_emit_both_documents_and_propagate_output_failure() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    ensure_ok(
      drive(taplo.execute_config(ConfigCommand::Default)),
      "default configuration rendering must succeed",
    )?;
    let default_output = stdout_text(&environment)?;
    ensure_eq(
      &default_output.as_str(),
      &"",
      "all-default configuration must serialize as the empty override document",
    )?;

    environment.clear_output();
    ensure_ok(
      drive(taplo.execute_config(ConfigCommand::Schema)),
      "configuration schema rendering must succeed",
    )?;
    let schema_output = stdout_text(&environment)?;
    let schema = ensure_ok(
      serde_json::from_str::<serde_json::Value>(&schema_output),
      "configuration schema output must be valid JSON",
    )?;
    let title = ensure_some(schema.get("title"), "the configuration schema must carry its title")?;
    ensure_eq(
      title,
      &serde_json::json!("Config"),
      "the configuration schema must identify the public configuration type",
    )?;
    ensure(
      schema.pointer("/properties/include").is_some(),
      "the configuration schema must expose file inclusion",
    )?;
    ensure(
      schema.pointer("/properties/formatting").is_some(),
      "the configuration schema must expose formatter policy",
    )?;

    environment.set_stdout_failure(true);
    let failure = drive(taplo.execute_config(ConfigCommand::Default));
    ensure(
      matches!(failure, Err(CliError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied),
      "configuration output must preserve typed stream failures",
    )
  }

  #[test]
  fn configuration_loading_discovers_prepares_caches_and_preserves_typed_failures() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    environment.insert_file("/workspace/taplo.toml", b"include = [\"configured/*.toml\"]\n".to_vec());
    environment.insert_file("/workspace/configured/value.toml", b"value=1\n".to_vec());
    environment.insert_file("/workspace/ignored.toml", b"ignored=1\n".to_vec());
    let automatic = config_general(None, false);
    let discovered = ensure_ok(
      drive(taplo.load_config(&automatic)),
      "automatic configuration discovery must load and prepare the nearest file",
    )?;
    ensure(
      (
        discovered.is_included(Path::new("/workspace/configured/value.toml")),
        discovered.is_included(Path::new("/workspace/ignored.toml")),
        environment.discovery_bases(),
      ) == (true, false, vec![PathBuf::from("/workspace")]),
      "the discovered configuration must prepare relative includes against the searched working directory",
    )?;

    let configured_files = ensure_ok(
      drive(
        taplo.collect_files(
          Path::new("/workspace"),
          &discovered,
          Vec::from([
            String::from("/workspace/configured/*.toml"),
            String::from("configured/*.toml"),
            String::from("/workspace/configured/*.toml"),
          ])
          .into_iter(),
        ),
      ),
      "absolute, relative, and duplicate CLI patterns must collect through one normalized set",
    )?;
    ensure(
      configured_files == [PathBuf::from("/workspace/configured/value.toml")],
      "file collection must deduplicate equivalent patterns and retain only configuration-included files",
    )?;
    let invalid_glob = ensure_some(
      drive(taplo.collect_files(Path::new("/workspace"), &discovered, Vec::from([String::from("[")]).into_iter())).err(),
      "an invalid CLI glob must fail before host enumeration",
    )?;
    ensure(
      matches!(invalid_glob, CliError::Glob { pattern, .. } if pattern == "/workspace/["),
      "file collection must retain the normalized rejected glob expression and typed parser source",
    )?;

    environment.insert_file("/workspace/taplo.toml", b"include = [\"replacement/*.toml\"]\n".to_vec());
    let cached = ensure_ok(
      drive(taplo.load_config(&automatic)),
      "a prepared invocation configuration must remain reusable",
    )?;
    ensure(
      (
        Arc::ptr_eq(&discovered, &cached),
        environment.discovery_bases(),
        cached.is_included(Path::new("/workspace/configured/value.toml")),
        cached.is_included(Path::new("/workspace/replacement/value.toml")),
      ) == (true, vec![PathBuf::from("/workspace")], true, false),
      "configuration reuse must return the prepared instance without rediscovery or mid-invocation drift",
    )?;

    let invalid_toml_environment = TestEnvironment::default();
    invalid_toml_environment.insert_file("/config/invalid.toml", b"include = [".to_vec());
    let mut invalid_toml_taplo = ensure_ok(
      Taplo::new(invalid_toml_environment),
      "the invalid-TOML configuration fixture must initialize",
    )?;
    let invalid_toml = ensure_some(
      drive(invalid_toml_taplo.load_config(&config_general(Some("/config/invalid.toml"), true))).err(),
      "invalid configuration TOML must fail decoding",
    )?;
    ensure(
      matches!(
        invalid_toml,
        CliError::ConfigDecode {
          path,
          ..
        } if path == PathBuf::from("/config/invalid.toml")
      ),
      "configuration decoding must retain the explicit source path and typed TOML error",
    )?;

    let invalid_utf8_environment = TestEnvironment::default();
    invalid_utf8_environment.insert_file("/config/non-utf8.toml", vec![0xff]);
    let mut invalid_utf8_taplo = ensure_ok(
      Taplo::new(invalid_utf8_environment),
      "the non-UTF-8 configuration fixture must initialize",
    )?;
    ensure(
      matches!(
        drive(invalid_utf8_taplo.load_config(&config_general(Some("/config/non-utf8.toml"), true,))),
        Err(CliError::Utf8(_))
      ),
      "configuration loading must preserve invalid UTF-8 as a distinct typed boundary",
    )?;

    let read_failure_environment = TestEnvironment::default();
    read_failure_environment.insert_file("/config/unreadable.toml", Vec::new());
    read_failure_environment.set_read_failure(true);
    let mut read_failure_taplo = ensure_ok(
      Taplo::new(read_failure_environment),
      "the unreadable configuration fixture must initialize",
    )?;
    ensure(
      matches!(
        drive(read_failure_taplo.load_config(&config_general(
          Some("/config/unreadable.toml"),
          true,
        ))),
        Err(CliError::Environment(
          taplo_common::environment::EnvironmentError::Io {
            operation: "read_file",
            source,
            ..
          }
        )) if source.kind() == std::io::ErrorKind::PermissionDenied
      ),
      "configuration loading must preserve the host read operation and I/O category",
    )?;

    let missing_cwd_environment = TestEnvironment::default();
    missing_cwd_environment.set_cwd(None);
    let mut missing_cwd_taplo = ensure_ok(
      Taplo::new(missing_cwd_environment),
      "the missing-working-directory configuration fixture must initialize",
    )?;
    ensure(
      matches!(
        drive(missing_cwd_taplo.load_config(&general())),
        Err(CliError::Failure(CliFailure::WorkingDirectoryRequired))
      ),
      "configuration preparation without an explicit base must require a working directory",
    )
  }

  #[test]
  fn stdin_formatting_writes_exact_output_and_enforces_check_mode() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    ensure_ok(
      drive(taplo.execute_format(stdin_format_command(&environment, b"value=1\n"))),
      "standard-input formatting must succeed",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"value = 1\n",
      "standard-input formatting must emit the complete normalized document",
    )?;

    environment.clear_output();
    let mut check_command = stdin_format_command(&environment, b"value = 1\n");
    check_command.output.check = true;
    ensure_ok(
      drive(taplo.execute_format(check_command.clone())),
      "check mode must accept an already formatted document",
    )?;
    ensure(
      environment.stdout().is_empty(),
      "successful check mode must not emit formatted output",
    )?;

    environment.set_stdin(b"value=1\n".to_vec());
    let mismatch = format_failure(
      &mut taplo,
      check_command,
      "check mode must return a typed mismatch for unformatted standard input",
    )?;
    ensure(
      matches!(mismatch, CliError::Failure(CliFailure::FormattingMismatch)),
      "check mode must reject an unformatted standard-input document",
    )?;

    environment.clear_output();
    let mut aligned = stdin_format_command(&environment, b"short=1\nlonger=2\n");
    aligned.options.push(String::from("align_entries=true"));
    ensure_ok(
      drive(taplo.execute_format(aligned)),
      "a valid command-line formatter option must override the default policy",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"short  = 1\nlonger = 2\n",
      "formatter option overrides must affect the emitted document",
    )
  }

  #[test]
  fn stdin_formatting_reports_malformed_input_and_supports_explicit_force() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    let blocked = format_failure(
      &mut taplo,
      stdin_format_command(&environment, b"value =\n"),
      "malformed standard-input formatting must return a typed failure",
    )?;
    ensure(
      matches!(blocked, CliError::Failure(CliFailure::FormattingBlocked)),
      "malformed standard input must be blocked by default",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "invalid TOML",
      "blocked formatting must emit the parser diagnostic",
    )?;
    ensure(
      environment.stdout().is_empty(),
      "blocked formatting must not emit a partial document",
    )?;

    environment.clear_output();
    let mut forced = stdin_format_command(&environment, b"value =\n");
    forced.input.force = true;
    ensure_ok(
      drive(taplo.execute_format(forced)),
      "explicit force must format around recoverable syntax diagnostics",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"value =\n",
      "forced formatting must preserve the malformed source span",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "invalid TOML",
      "forced formatting must still report the syntax diagnostic",
    )
  }

  #[test]
  fn file_formatting_updates_only_changed_files_and_check_diff_never_writes() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    environment.insert_file("/workspace/changed.toml", b"value=1\n".to_vec());
    environment.insert_file("/workspace/stable.toml", b"stable = true\n".to_vec());
    ensure_ok(
      drive(taplo.execute_format(format_command(Vec::from([String::from("*.toml")])))),
      "file formatting must process selected deterministic files",
    )?;
    ensure(
      ensure_ok(
        environment.read_file(Path::new("/workspace/changed.toml")),
        "the changed file must remain readable",
      )? == b"value = 1\n",
      "file formatting must persist the normalized changed document",
    )?;
    ensure(
      ensure_ok(
        environment.read_file(Path::new("/workspace/stable.toml")),
        "the stable file must remain readable",
      )? == b"stable = true\n",
      "file formatting must leave an already formatted document unchanged",
    )?;
    ensure(
      environment.writes() == [PathBuf::from("/workspace/changed.toml")],
      "only changed files must be written",
    )?;

    let (check_environment, mut check_taplo) = initialized_cli()?;
    check_environment.insert_file("/workspace/input.toml", b"value=1\n".to_vec());
    let mut check = format_command(Vec::from([String::from("input.toml")]));
    check.output.check = true;
    check.output.diff = true;
    let result = drive(check_taplo.execute_format(check));
    ensure(
      matches!(result, Err(CliError::Failure(CliFailure::FileFormattingFailed))),
      "check mode must aggregate an unformatted-file failure",
    )?;
    ensure(
      check_environment.writes().is_empty(),
      "check-plus-diff mode must never modify the input file",
    )?;
    let diff = stdout_text(&check_environment)?;
    ensure_contains(&diff, "diff a/", "diff mode must emit the selected file header")?;
    ensure_contains(&diff, "-value=1", "diff mode must retain the removed source line")?;
    ensure_contains(&diff, "+value = 1", "diff mode must retain the inserted formatted line")
  }

  #[test]
  fn file_formatting_aggregates_malformed_inputs_and_force_preserves_diagnostics() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    environment.insert_file("/workspace/broken.toml", b"value=\n".to_vec());
    environment.insert_file("/workspace/changed.toml", b"changed=1\n".to_vec());
    let mut command = format_command(Vec::from([String::from("*.toml")]));
    command.stdin_filepath = Some(String::from("/ignored/stdin.toml"));
    let aggregate_failure = format_failure(
      &mut taplo,
      command,
      "malformed file formatting must return the aggregate typed failure",
    )?;
    ensure(
      matches!(aggregate_failure, CliError::Failure(CliFailure::FileFormattingFailed)),
      "one malformed file must fail the aggregate command without aborting later files",
    )?;
    ensure(
      ensure_ok(
        environment.read_file(Path::new("/workspace/broken.toml")),
        "the blocked malformed file must remain readable",
      )? == b"value=\n",
      "default file formatting must leave malformed input unchanged",
    )?;
    ensure(
      ensure_ok(
        environment.read_file(Path::new("/workspace/changed.toml")),
        "the valid sibling file must remain readable",
      )? == b"changed = 1\n",
      "aggregate failure must not prevent a later valid file from being formatted",
    )?;
    let diagnostics = stderr_text(&environment)?;
    ensure_contains(
      &diagnostics,
      "/workspace/broken.toml",
      "file diagnostics must identify the real selected path",
    )?;
    ensure_lacks(
      &diagnostics,
      "/ignored/stdin.toml",
      "a standard-input identity must not replace file diagnostic paths",
    )?;

    let (forced_environment, mut forced_taplo) = initialized_cli()?;
    forced_environment.insert_file("/workspace/broken.toml", b"value=\n".to_vec());
    let mut forced = format_command(Vec::from([String::from("broken.toml")]));
    forced.input.force = true;
    ensure_ok(
      drive(forced_taplo.execute_format(forced)),
      "explicit force must format a file around recoverable syntax diagnostics",
    )?;
    ensure(
      ensure_ok(
        forced_environment.read_file(Path::new("/workspace/broken.toml")),
        "the forced malformed file must remain readable",
      )? == b"value=\n",
      "forced file formatting must preserve a source consisting entirely of the retained error span",
    )?;
    ensure(
      forced_environment.writes().is_empty(),
      "forced formatting must not rewrite a malformed file when its recoverable output is unchanged",
    )?;
    ensure_contains(
      &stderr_text(&forced_environment)?,
      "invalid TOML",
      "forced file formatting must still report its syntax diagnostic",
    )
  }

  #[test]
  fn formatting_validates_input_identity_options_and_host_failures() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    let mut relative = stdin_format_command(&environment, b"value =\n");
    relative.stdin_filepath = Some(String::from("nested/input.toml"));
    let relative_failure = format_failure(
      &mut taplo,
      relative,
      "malformed relative standard input must return a typed failure",
    )?;
    ensure(
      matches!(relative_failure, CliError::Failure(CliFailure::FormattingBlocked)),
      "a malformed standard-input document with a relative identity must remain blocked",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "/workspace/nested/input.toml",
      "a relative standard-input identity must resolve against the current working directory",
    )?;

    environment.clear_output();
    let mut absolute = stdin_format_command(&environment, b"value =\n");
    absolute.stdin_filepath = Some(String::from("/virtual/input.toml"));
    let absolute_failure = format_failure(
      &mut taplo,
      absolute,
      "malformed absolute standard input must return a typed failure",
    )?;
    ensure(
      matches!(absolute_failure, CliError::Failure(CliFailure::FormattingBlocked)),
      "a malformed standard-input document with an absolute identity must remain blocked",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "/virtual/input.toml",
      "an absolute standard-input identity must be preserved in diagnostics",
    )?;
    ensure_lacks(
      &stderr_text(&environment)?,
      "/workspace/virtual/input.toml",
      "an absolute standard-input identity must not be joined to the current working directory",
    )?;

    let (missing_cwd, mut missing_cwd_taplo) = initialized_cli()?;
    missing_cwd.set_cwd(None);
    let mut unresolved = stdin_format_command(&missing_cwd, b"value = 1\n");
    unresolved.stdin_filepath = Some(String::from("relative.toml"));
    let unresolved_failure = format_failure(
      &mut missing_cwd_taplo,
      unresolved,
      "an unresolved standard-input identity must return a typed failure",
    )?;
    ensure(
      matches!(unresolved_failure, CliError::Failure(CliFailure::WorkingDirectoryRequired)),
      "a relative standard-input identity must fail when no working directory can resolve it",
    )?;

    let mut reject_option = |option: &str, context: &'static str| {
      let mut command = stdin_format_command(&environment, b"value=1\n");
      command.options.push(option.to_owned());
      format_failure(&mut taplo, command, context)
    };
    let option_failure = reject_option("not-an-assignment", "an invalid formatter-option shape must return a typed failure")?;
    let value_failure = reject_option(
      "align_entries=not-a-bool",
      "an invalid formatter-option value must return a typed failure",
    )?;
    let option_observation = match option_failure {
      CliError::FormatOption(taplo::formatter::OptionParseError::InvalidOption(option)) => Some(option),
      _ => None,
    };
    let value_observation = match value_failure {
      CliError::FormatOption(taplo::formatter::OptionParseError::InvalidValue {
        key,
        input,
        expected,
        ..
      }) => Some((key, input, expected)),
      _ => None,
    };
    ensure(
      (option_observation, value_observation)
        == (
          Some(String::from("not-an-assignment")),
          Some((String::from("align_entries"), String::from("not-a-bool"), "bool")),
        ),
      "formatter option failures must distinguish malformed assignments from invalid typed values",
    )?;

    let (missing_cwd, mut missing_cwd_taplo) = initialized_cli()?;
    missing_cwd.set_cwd(None);
    let cwd_failure = format_failure(
      &mut missing_cwd_taplo,
      format_command(Vec::new()),
      "file formatting without a current directory must return a typed failure",
    )?;
    ensure(
      matches!(cwd_failure, CliError::Failure(CliFailure::WorkingDirectoryRequired)),
      "file formatting must require a current working directory",
    )?;

    let (output_failure, mut output_taplo) = initialized_cli()?;
    let output_command = stdin_format_command(&output_failure, b"value=1\n");
    output_failure.set_stdout_failure(true);
    let stream_failure = format_failure(
      &mut output_taplo,
      output_command,
      "formatter output failure must cross the CLI boundary",
    )?;
    ensure(
      matches!(
        stream_failure,
        CliError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
      ),
      "formatted standard output must propagate its typed stream failure",
    )
  }

  #[test]
  fn queries_render_value_json_and_toml_contracts() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    let source = b"name = \"taplo\"\nvalues = [1, 2]\n[table]\nkey = \"value\"\n".to_vec();

    environment.set_stdin(source.clone());
    ensure_ok(
      drive(taplo.execute_get(get_command(Some("name")))),
      "scalar value queries must succeed",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"taplo\n",
      "scalar value queries must strip TOML quoting",
    )?;

    environment.clear_output();
    environment.set_stdin(source.clone());
    let mut array_query = get_command(Some("values"));
    array_query.separator = Some(String::from(","));
    ensure_ok(
      drive(taplo.execute_get(array_query)),
      "array value queries must support an explicit separator",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"1,2\n",
      "array values must retain order and separator policy",
    )?;

    environment.clear_output();
    environment.set_stdin(source.clone());
    let mut json_query = get_command(Some("table"));
    json_query.output_format = OutputFormat::Json;
    json_query.strip_newline = true;
    ensure_ok(drive(taplo.execute_get(json_query)), "JSON table queries must succeed")?;
    let json = ensure_ok(
      serde_json::from_slice::<serde_json::Value>(&environment.stdout()),
      "JSON query output must be valid JSON",
    )?;
    ensure_eq(
      &json,
      &serde_json::json!({"key": "value"}),
      "JSON query output must preserve the selected table",
    )?;

    ensure_get_output(
      &environment,
      &taplo,
      &source,
      formatted_get_command(Some("table"), OutputFormat::Toml),
      "key = \"value\"\n",
      "TOML table queries must succeed",
      "TOML query output must emit the complete selected table",
    )
  }

  #[test]
  fn queries_render_whole_documents_multi_matches_and_scalar_families() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    let source = b"item_a = 1\nitem_b = 2\nother = 3\n".to_vec();

    environment.set_stdin(source.clone());
    let mut whole_json = get_command(None);
    whole_json.output_format = OutputFormat::Json;
    ensure_ok(drive(taplo.execute_get(whole_json)), "whole-document JSON output must succeed")?;
    let whole_json_output = stdout_text(&environment)?;
    ensure(
      whole_json_output.ends_with('\n'),
      "whole-document JSON output must retain the default trailing newline",
    )?;
    ensure_eq(
      &ensure_ok(
        serde_json::from_str::<serde_json::Value>(&whole_json_output),
        "whole-document JSON output must decode",
      )?,
      &serde_json::json!({
        "item_a": 1,
        "item_b": 2,
        "other": 3
      }),
      "whole-document JSON output must preserve every root entry",
    )?;

    environment.clear_output();
    environment.set_stdin(source.clone());
    let mut multi_json = get_command(Some("item_*"));
    multi_json.output_format = OutputFormat::Json;
    ensure_ok(drive(taplo.execute_get(multi_json)), "multi-match JSON output must succeed")?;
    ensure_eq(
      &ensure_ok(
        serde_json::from_slice::<serde_json::Value>(&environment.stdout()),
        "multi-match JSON output must decode",
      )?,
      &serde_json::json!([1, 2]),
      "multi-match JSON output must use an ordered list envelope",
    )?;

    ensure_get_output(
      &environment,
      &taplo,
      &source,
      formatted_get_command(Some("item_*"), OutputFormat::Toml),
      "[\n  1,\n  2,\n]\n",
      "multi-match TOML output must succeed",
      "multi-match TOML output must retain its ordered list envelope and trailing newline",
    )?;

    for (pattern, expected, context) in [
      (
        ".enabled",
        "true\n",
        "leading-dot Boolean queries must normalize and render their scalar value",
      ),
      ("ratio", "1.5\n", "floating-point queries must render their canonical scalar value"),
      (
        "timestamp",
        "1979-05-27T07:32:00Z\n",
        "date-time queries must render their semantic scalar value",
      ),
    ] {
      environment.clear_output();
      environment.set_stdin(b"enabled = true\nratio = 1.5\ntimestamp = 1979-05-27T07:32:00Z\n".to_vec());
      ensure_ok(drive(taplo.execute_get(get_command(Some(pattern)))), context)?;
      ensure_eq(&stdout_text(&environment)?.as_str(), &expected, context)?;
    }

    environment.clear_output();
    environment.set_stdin(b"value = 1\n".to_vec());
    let mut whole_toml = get_command(None);
    whole_toml.output_format = OutputFormat::Toml;
    whole_toml.strip_newline = true;
    ensure_ok(
      drive(taplo.execute_get(whole_toml)),
      "whole-document TOML output must support newline stripping",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"value = 1",
      "whole-document TOML output must remove only its trailing newline",
    )
  }

  #[test]
  fn queries_reject_incompatible_missing_table_and_invalid_documents() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    environment.set_stdin(b"value = 1\n".to_vec());
    let mut invalid_separator = get_command(Some("value"));
    invalid_separator.output_format = OutputFormat::Json;
    invalid_separator.separator = Some(String::from(","));
    ensure(
      matches!(
        drive(taplo.execute_get(invalid_separator)),
        Err(CliError::Failure(CliFailure::InvalidSeparator))
      ),
      "a separator must be rejected for non-value output",
    )?;

    for (source, pattern, expected, context) in [
      (
        "value = 1\n",
        "missing",
        CliFailure::NoQueryMatches,
        "a query with no matches must retain its typed command failure",
      ),
      (
        "[table]\nvalue = 1\n",
        "table",
        CliFailure::TableValueOutput,
        "value output must reject table nodes",
      ),
    ] {
      environment.set_stdin(source.as_bytes().to_vec());
      ensure(
        matches!(
          drive(taplo.execute_get(get_command(Some(pattern)))),
          Err(CliError::Failure(actual)) if actual == expected
        ),
        context,
      )?;
    }

    environment.set_stdin(b"value = 1\n".to_vec());
    ensure(
      matches!(
        drive(taplo.execute_get(get_command(None))),
        Err(CliError::Failure(CliFailure::TableValueOutput))
      ),
      "whole-document value output must reject the root table rather than inventing a scalar representation",
    )?;

    for (source, expected, diagnostic, failure_context, diagnostic_context) in [
      (
        "value =\n",
        CliFailure::SyntaxErrors,
        "invalid TOML",
        "queries must reject syntax-invalid input",
        "syntax-invalid queries must emit diagnostics",
      ),
      (
        "value = 1\nvalue = 2\n",
        CliFailure::SemanticErrors,
        "conflicting keys",
        "queries must reject semantically conflicting input",
        "semantic query failures must emit diagnostics",
      ),
    ] {
      environment.clear_output();
      environment.set_stdin(source.as_bytes().to_vec());
      ensure(
        matches!(
          drive(taplo.execute_get(get_command(Some("value")))),
          Err(CliError::Failure(actual)) if actual == expected
        ),
        failure_context,
      )?;
      ensure_contains(&stderr_text(&environment)?, diagnostic, diagnostic_context)?;
    }
    Ok(())
  }

  #[test]
  fn queries_read_files_strip_newlines_and_propagate_output_failures() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    environment.insert_file("/workspace/query.toml", b"value = \"file\"\n".to_vec());
    let mut file_query = get_command(Some("value"));
    file_query.file_path = Some(PathBuf::from("/workspace/query.toml"));
    file_query.strip_newline = true;
    ensure_ok(
      drive(taplo.execute_get(file_query)),
      "queries must read an explicitly selected file",
    )?;
    ensure_eq(
      &stdout_text(&environment)?.as_str(),
      &"file",
      "strip-newline mode must omit the final line feed",
    )?;

    environment.clear_output();
    environment.set_stdin(b"value = 1\n".to_vec());
    environment.set_stdout_failure(true);
    ensure(
      matches!(
        drive(taplo.execute_get(get_command(Some("value")))),
        Err(CliError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied
      ),
      "query output must propagate its typed stream failure",
    )?;
    ensure_lacks(
      &stderr_text(&environment)?,
      "invalid TOML",
      "a transport failure after a valid query must not fabricate diagnostics",
    )
  }

  #[cfg(feature = "toml-test")]
  #[test]
  fn toml_test_serializes_scalar_families_and_nested_containers() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    environment.set_stdin(
      br#"string = "value"
integer = 7
float = 1.5
boolean = true
offset = 1979-05-27T07:32:00Z
local_datetime = 1979-05-27T07:32:00
local_date = 1979-05-27
local_time = 07:32:00
array = [1, "two"]
"#
      .to_vec(),
    );
    ensure_ok(drive(taplo.execute_toml_test()), "valid TOML-test input must serialize")?;
    let output = ensure_ok(
      serde_json::from_slice::<serde_json::Value>(&environment.stdout()),
      "TOML-test output must be valid JSON",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/string/type"), "string type must be present")?,
      &serde_json::json!("string"),
      "strings must use the TOML-test string label",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/integer/value"), "integer value must be present")?,
      &serde_json::json!("7"),
      "integers must use their canonical textual value",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/offset/type"), "offset date-time type must be present")?,
      &serde_json::json!("datetime"),
      "offset date-times must use the TOML-test datetime label",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/local_datetime/type"), "local date-time type must be present")?,
      &serde_json::json!("datetime-local"),
      "local date-times must retain their distinct label",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/local_date/type"), "local date type must be present")?,
      &serde_json::json!("date-local"),
      "local dates must retain their distinct label",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/local_time/type"), "local time type must be present")?,
      &serde_json::json!("time-local"),
      "local times must retain their distinct label",
    )?;
    ensure_eq(
      ensure_some(output.pointer("/array/1/value"), "nested array value must be present")?,
      &serde_json::json!("two"),
      "nested containers must recursively use the TOML-test envelope",
    )
  }

  #[cfg(feature = "toml-test")]
  #[test]
  fn toml_test_rejects_syntax_semantics_and_output_failures() -> Result<(), TestFailure> {
    let (environment, taplo) = initialized_cli()?;
    environment.set_stdin(b"value =\n".to_vec());
    ensure(
      matches!(
        drive(taplo.execute_toml_test()),
        Err(CliError::Failure(CliFailure::InvalidTomlTestInput))
      ),
      "TOML-test must reject syntax-invalid input",
    )?;
    ensure(
      !environment.stderr().is_empty(),
      "syntax-invalid TOML-test input must emit diagnostics",
    )?;

    environment.clear_output();
    environment.set_stdin(b"value = 1\nvalue = 2\n".to_vec());
    ensure(
      matches!(
        drive(taplo.execute_toml_test()),
        Err(CliError::Failure(CliFailure::InvalidTomlTestInput))
      ),
      "TOML-test must reject semantically conflicting input",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "conflicting keys",
      "semantic TOML-test failures must retain their diagnostic",
    )?;

    environment.clear_output();
    environment.set_stdin(b"value = 1\n".to_vec());
    environment.set_stdout_failure(true);
    ensure(
      matches!(
        drive(taplo.execute_toml_test()),
        Err(CliError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied
      ),
      "TOML-test output must propagate its typed stream failure",
    )
  }

  #[cfg(feature = "lint")]
  #[test]
  fn lint_accepts_clean_input_and_reports_syntax_and_semantic_failures() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    environment.set_stdin(b"value = 1\n".to_vec());
    ensure_ok(
      drive(taplo.execute_lint(lint_command())),
      "schema-disabled lint must accept clean standard input",
    )?;
    ensure(environment.stderr().is_empty(), "clean lint input must not emit diagnostics")?;

    environment.set_stdin(b"value =\n".to_vec());
    ensure(
      matches!(
        drive(taplo.execute_lint(lint_command())),
        Err(CliError::Failure(CliFailure::SyntaxErrors))
      ),
      "lint must reject syntax-invalid input",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "invalid TOML",
      "syntax-invalid lint input must emit parser diagnostics",
    )?;

    environment.clear_output();
    environment.set_stdin(b"value = 1\nvalue = 2\n".to_vec());
    ensure(
      matches!(
        drive(taplo.execute_lint(lint_command())),
        Err(CliError::Failure(CliFailure::SemanticErrors))
      ),
      "lint must reject semantic conflicts",
    )?;
    ensure_contains(
      &stderr_text(&environment)?,
      "conflicting keys",
      "semantic lint failures must emit structural diagnostics",
    )
  }

  #[cfg(feature = "lint")]
  #[test]
  fn lint_validates_explicit_schema_and_aggregates_file_failures() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    install_required_string_schema(&environment);
    environment.set_stdin(b"name = 7\n".to_vec());
    let mut schema_command = lint_command();
    schema_command.no_schema = false;
    schema_command.schema = Some(ensure_ok(
      url::Url::parse("file:///workspace/schema.json"),
      "the schema fixture URL must parse",
    )?);
    ensure(
      matches!(
        drive(taplo.execute_lint(schema_command.clone())),
        Err(CliError::Failure(CliFailure::SchemaValidation))
      ),
      "an explicit schema must reject a mismatched document",
    )?;
    ensure(!environment.stderr().is_empty(), "schema validation failure must emit diagnostics")?;

    environment.clear_output();
    environment.set_stdin(b"name = \"taplo\"\n".to_vec());
    ensure_ok(
      drive(taplo.execute_lint(schema_command)),
      "an explicit schema must accept a matching document",
    )?;
    ensure(
      environment.stderr().is_empty(),
      "successful schema validation must emit no diagnostics",
    )?;

    let (file_environment, mut file_taplo) = initialized_cli()?;
    file_environment.insert_file("/workspace/invalid.toml", b"value =\n".to_vec());
    let mut files = lint_command();
    files.files = Vec::from([String::from("invalid.toml")]);
    let result = drive(file_taplo.execute_lint(files));
    ensure(
      matches!(result, Err(CliError::Failure(CliFailure::FileValidationFailed))),
      "file linting must aggregate individual validation failures",
    )?;
    ensure_contains(
      &stderr_text(&file_environment)?,
      "invalid TOML",
      "invalid files must emit their source diagnostics",
    )
  }

  #[cfg(feature = "lint")]
  #[test]
  fn lint_file_mode_and_schema_configuration_preserve_host_and_policy_boundaries() -> Result<(), TestFailure> {
    let (file_environment, mut file_taplo) = initialized_cli()?;
    file_environment.insert_file("/workspace/valid.toml", b"value = 1\n".to_vec());
    let mut valid_file = lint_command();
    valid_file.files = Vec::from([String::from("valid.toml")]);
    ensure_ok(
      drive(file_taplo.execute_lint(valid_file)),
      "file linting must accept a clean selected file",
    )?;
    ensure(
      file_environment.stderr().is_empty(),
      "successful file linting must not emit diagnostics",
    )?;

    let (missing_cwd, mut missing_cwd_taplo) = initialized_cli()?;
    missing_cwd.set_cwd(None);
    let mut unresolved_file = lint_command();
    unresolved_file.files = Vec::from([String::from("relative.toml")]);
    ensure(
      matches!(
        drive(missing_cwd_taplo.execute_lint(unresolved_file)),
        Err(CliError::Failure(CliFailure::WorkingDirectoryRequired))
      ),
      "file linting must require a current directory for relative selection",
    )?;

    let (invalid_utf8, mut invalid_utf8_taplo) = initialized_cli()?;
    invalid_utf8.insert_file("/workspace/non-utf8.toml", vec![0xff]);
    let mut invalid_file = lint_command();
    invalid_file.files = Vec::from([String::from("non-utf8.toml")]);
    ensure(
      matches!(
        drive(invalid_utf8_taplo.execute_lint(invalid_file)),
        Err(CliError::Failure(CliFailure::FileValidationFailed))
      ),
      "file linting must aggregate a selected file's typed UTF-8 failure",
    )?;

    let (disabled_environment, mut disabled_taplo) = initialized_cli()?;
    install_required_string_schema(&disabled_environment);
    disabled_environment.insert_file(
      "/workspace/disabled-schema.toml",
      b"[schema]\nenabled = false\npath = \"/workspace/schema.json\"\n".to_vec(),
    );
    disabled_environment.set_stdin(b"name = 7\n".to_vec());
    let mut disabled_schema = lint_command();
    disabled_schema.general = config_general(Some("/workspace/disabled-schema.toml"), true);
    disabled_schema.no_schema = false;
    ensure_ok(
      drive(disabled_taplo.execute_lint(disabled_schema)),
      "configuration-disabled schema validation must accept a document that its configured schema would reject",
    )?;
    ensure(
      disabled_environment.stderr().is_empty(),
      "configuration-disabled schema validation must not emit schema diagnostics",
    )?;

    let (catalog_environment, mut catalog_taplo) = initialized_cli()?;
    install_required_string_schema(&catalog_environment);
    catalog_environment.insert_file(
      "/workspace/catalog.json",
      br#"{"schemas":[{"title":"fixture","description":"","url":"file:///workspace/schema.json","urlHash":"","authors":[],"version":null,"patterns":[".*\\.toml$"]}]}"#.to_vec(),
    );
    catalog_environment.set_stdin(b"name = 7\n".to_vec());
    let mut catalog_schema = lint_command();
    catalog_schema.no_schema = false;
    catalog_schema.schema_catalog = Vec::from([ensure_ok(
      url::Url::parse("file:///workspace/catalog.json"),
      "the file-backed schema catalog URL must parse",
    )?]);
    ensure(
      matches!(
        drive(catalog_taplo.execute_lint(catalog_schema.clone())),
        Err(CliError::Failure(CliFailure::SchemaValidation))
      ),
      "a command-selected schema catalog must reject a mismatched document",
    )?;
    catalog_environment.clear_output();
    catalog_environment.set_stdin(b"name = \"taplo\"\n".to_vec());
    ensure_ok(
      drive(catalog_taplo.execute_lint(catalog_schema)),
      "a command-selected schema catalog must accept a matching document",
    )?;
    ensure(
      catalog_environment.stderr().is_empty(),
      "successful catalog-backed validation must not emit diagnostics",
    )
  }

  #[test]
  fn dispatcher_applies_all_color_policies() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    let mut automatic = arguments(TaploCommand::Config {
      cmd: ConfigCommand::Default,
    });
    automatic.colors = Colors::Auto;
    ensure_ok(drive(taplo.execute_local(automatic)), "automatic color selection must execute")?;
    ensure(!taplo.colors, "the deterministic non-terminal host must disable automatic colors")?;

    environment.clear_output();
    let mut always = arguments(TaploCommand::Config {
      cmd: ConfigCommand::Default,
    });
    always.colors = Colors::Always;
    ensure_ok(drive(taplo.execute_local(always)), "forced color selection must execute")?;
    ensure(taplo.colors, "the always policy must enable colors")?;

    environment.clear_output();
    ensure_ok(
      drive(taplo.execute_local(arguments(TaploCommand::Config {
        cmd: ConfigCommand::Default,
      }))),
      "disabled color selection must execute",
    )?;
    ensure(!taplo.colors, "the never policy must disable colors")
  }

  #[cfg(feature = "completions")]
  #[test]
  fn dispatcher_rejects_an_unknown_completion_shell() -> Result<(), TestFailure> {
    let (_, mut taplo) = initialized_cli()?;
    let result = drive(taplo.execute_local(arguments(TaploCommand::Completions {
      shell: String::from("unknown-shell"),
    })));
    ensure(
      matches!(result, Err(CliError::InvalidShell { shell, .. }) if shell == "unknown-shell"),
      "completion dispatch must retain the rejected shell in its typed error",
    )
  }

  #[cfg(feature = "lsp")]
  #[test]
  fn local_dispatcher_rejects_concurrent_command() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let mut taplo = ensure_ok(Taplo::new(environment), "local CLI state must initialize")?;
    let result = drive(taplo.execute_local(arguments(TaploCommand::Lsp {
      cmd: stdio_lsp_command()
    })));
    ensure(
      matches!(result, Err(CliError::Failure(CliFailure::ConcurrentEnvironmentRequired))),
      "the local dispatcher must preserve the typed concurrent-capability boundary",
    )
  }

  #[cfg(feature = "lsp")]
  #[test]
  fn full_dispatcher_completes_a_bounded_protocol_transaction() -> Result<(), TestFailure> {
    let (environment, mut taplo) = initialized_cli()?;
    let bounded_client = BoundedLspClient {
      input:    environment.interactive_stdin(),
      output:   tokio::io::BufReader::new(environment.stdout_reader()),
      observed: Vec::new(),
    };
    let runtime = ensure_ok(
      tokio::runtime::Builder::new_current_thread().enable_all().build(),
      "the native LSP transaction runtime must initialize",
    )?;

    runtime.block_on(async {
      let command = taplo.execute(arguments(TaploCommand::Lsp {
        cmd: stdio_lsp_command()
      }));
      let client = async move {
        let mut client = bounded_client;
        let document_uri = "file:///workspace/document.toml";
        let document = serde_json::json!({
          "textDocument": {
            "uri": document_uri
          }
        });
        let positioned = serde_json::json!({
          "textDocument": {
            "uri": document_uri
          },
          "position": {
            "line": 0,
            "character": 1
          }
        });

        client
          .send_request(
            -1,
            "textDocument/hover",
            Some(positioned.clone()),
            "the bounded client must send its pre-initialization request",
          )
          .await?;
        let rejected = client.response(-1).await?;
        ensure(
          rejected.result.is_none(),
          "a pre-initialization rejection must not fabricate a success result",
        )?;
        let initialization_error = ensure_some(rejected.error, "the pre-initialization request must carry its typed RPC error")?;
        ensure_eq(
          &initialization_error,
          &rpc::RpcError::server_not_initialized(),
          "the CLI server must reject ordinary work before initialization",
        )?;

        let _text_document_sync = client
          .request_selected(
            0,
            "initialize",
            Some(serde_json::json!({
              "processId": null,
              "rootUri": null,
              "capabilities": {},
              "workspaceFolders": []
            })),
            "/capabilities/textDocumentSync",
            "the CLI server must advertise full document synchronization",
          )
          .await?;

        client
          .notify(
            "workspace/didChangeConfiguration",
            Some(serde_json::json!({
              "settings": {
                "schema": {
                  "catalogs": []
                }
              }
            })),
            "the bounded client must push deterministic configuration",
          )
          .await?;
        client
          .request_selected_where(
            1,
            "taplo/listSchemas",
            Some(serde_json::json!({
              "documentUri": document_uri
            })),
            "/schemas",
            "the request after pushed configuration must observe a schema collection",
            json_array_contract,
          )
          .await?;

        client
          .notify(
            "textDocument/didOpen",
            Some(serde_json::json!({
              "textDocument": {
                "uri": document_uri,
                "languageId": "toml",
                "version": 1,
                "text": "name=\"taplo\"\nvalues = [1, 2]\n"
              }
            })),
            "the bounded client must open its document",
          )
          .await?;
        client
          .request_selected_where(
            2,
            "textDocument/foldingRange",
            Some(document.clone()),
            "",
            "the opened document must produce a folding-range collection",
            json_array_contract,
          )
          .await?;
        ensure(
          client
            .observed
            .iter()
            .any(|message| message.method.as_deref() == Some("textDocument/publishDiagnostics")),
          "opening the document must publish replacement diagnostics through standard output",
        )?;

        client
          .request_selected_where(
            3,
            "textDocument/documentSymbol",
            Some(document.clone()),
            "",
            "the opened document must expose its named symbols",
            non_empty_json_array_contract,
          )
          .await?;

        client
          .request_selected_where(
            4,
            "textDocument/formatting",
            Some(serde_json::json!({
              "textDocument": {
                "uri": document_uri
              },
              "options": {
                "tabSize": 2,
                "insertSpaces": true
              }
            })),
            "",
            "the unformatted opened document must produce a replacement edit",
            non_empty_json_array_contract,
          )
          .await?;

        for (id, method, params, absence_context) in [
          (
            5,
            "textDocument/completion",
            positioned.clone(),
            "completion without an effective schema must remain absent",
          ),
          (
            6,
            "textDocument/hover",
            positioned.clone(),
            "hover without an effective schema must remain absent",
          ),
          (
            7,
            "textDocument/documentLink",
            document.clone(),
            "document links without an effective schema must remain absent",
          ),
        ] {
          client
            .request_selected_where(id, method, Some(params), "", absence_context, json_null_contract)
            .await?;
        }

        client
          .request_selected_where(
            8,
            "textDocument/semanticTokens/full",
            Some(document.clone()),
            "/data",
            "semantic-token output must retain the registered wire data collection",
            json_array_contract,
          )
          .await?;

        let prepare_rename = client
          .request(
            9,
            "textDocument/prepareRename",
            Some(positioned.clone()),
            "an identifier position must prepare a rename target",
          )
          .await?;
        ensure(!prepare_rename.is_null(), "an identifier position must prepare a rename target")?;

        let _changes = client
          .request_selected(
            10,
            "textDocument/rename",
            Some(serde_json::json!({
              "textDocument": {
                "uri": document_uri
              },
              "position": {
                "line": 0,
                "character": 1
              },
              "newName": "renamed"
            })),
            "/changes",
            "rename must return a workspace edit for the selected identifier",
          )
          .await?;

        for (id, method, source_text, expected_text, conversion_context) in [
          (
            11,
            "taplo/convertToJson",
            "name = \"taplo\"\n",
            "\"name\"",
            "TOML-to-JSON conversion must return converted text",
          ),
          (
            12,
            "taplo/convertToToml",
            "{\"name\":\"taplo\"}",
            "name",
            "JSON-to-TOML conversion must return converted text",
          ),
        ] {
          client
            .request_text_containing(id, method, source_text, expected_text, conversion_context)
            .await?;
        }

        client
          .request_selected_where(
            13,
            "taplo/associatedSchema",
            Some(serde_json::json!({
              "documentUri": document_uri
            })),
            "/schema",
            "the schema-disabled transaction must report no effective schema",
            json_null_contract,
          )
          .await?;

        client
          .request_selected_where(
            99,
            "shutdown",
            None,
            "",
            "shutdown must emit the standard null result",
            json_null_contract,
          )
          .await?;
        client
          .notify("exit", None, "the bounded client must send terminal exit")
          .await?;
        ensure_ok(
          tokio::io::AsyncWriteExt::shutdown(&mut client.input).await,
          "the bounded client must close standard input after terminal exit",
        )
      };

      tokio::pin!(command);
      tokio::pin!(client);
      tokio::select! {
        command_result = &mut command => {
          ensure_ok(command_result, "the full CLI dispatcher must complete its LSP transaction")?;
          client.await
        }
        client_result = &mut client => {
          client_result?;
          ensure_ok(command.await, "the full CLI dispatcher must complete after terminal exit")
        }
      }
    })?;

    ensure(
      environment.stderr().is_empty(),
      "the successful bounded LSP transaction must not emit command diagnostics",
    )
  }
}
