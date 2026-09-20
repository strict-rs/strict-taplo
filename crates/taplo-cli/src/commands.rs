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
  use std::fmt::Debug;
  #[cfg(feature = "lsp")]
  use std::io;
  use std::io::ErrorKind;
  use std::iter::once;
  #[cfg(feature = "lsp")]
  use std::num::ParseIntError;
  use std::path::Path;
  use std::path::PathBuf;
  use std::str::from_utf8;
  use std::sync::Arc;

  #[cfg(feature = "lsp")]
  use futures::future::Either;
  #[cfg(feature = "lsp")]
  use futures::future::select;
  #[cfg(feature = "lsp")]
  use strict_test_support::OptionFailure;
  #[cfg(feature = "lsp")]
  use strict_test_support::PredicateFailure;
  #[cfg(feature = "lsp")]
  use strict_test_support::ensure_some;
  use strict_test_support::ensure_that;
  use taplo::formatter::OptionParseError;
  use taplo_common::config::Config;
  use taplo_common::environment::EnvironmentError;
  #[cfg(feature = "lsp")]
  use taplo_lsp_async::rpc;
  use taplo_test_support::drive;
  #[cfg(feature = "lsp")]
  use tokio::io::AsyncBufReadExt;
  #[cfg(feature = "lsp")]
  use tokio::io::AsyncReadExt;
  #[cfg(feature = "lsp")]
  use tokio::io::AsyncWriteExt;
  #[cfg(feature = "lsp")]
  use tokio::io::BufReader;
  #[cfg(feature = "lsp")]
  use tokio::runtime::Builder;

  use crate::CliError;
  use crate::CliFailure;
  #[cfg(feature = "lsp")]
  use crate::LocalCommandFuture;
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
  #[cfg(feature = "lsp")]
  use crate::args::LspCommand;
  #[cfg(feature = "lsp")]
  use crate::args::LspCommandIo;
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
  fn stdio_lsp_command() -> LspCommand {
    LspCommand {
      general: general(),
      io:      LspCommandIo::Stdio {},
    }
  }

  /// Complete outbound request, serialization, and native pipe-write result.
  #[cfg(feature = "lsp")]
  #[derive(Debug)]
  struct SentMessage {
    /// Typed JSON-RPC request or notification.
    request: rpc::Request<serde_json::Value>,
    /// Complete framed bytes or native serialization failure.
    frame:   Result<Vec<u8>, serde_json::Error>,
    /// Native write result when serialization succeeded.
    written: Option<io::Result<()>>,
  }

  /// Header, body, and native results observed while reading one response frame.
  #[cfg(feature = "lsp")]
  #[derive(Debug)]
  struct ResponseFrame {
    /// Complete header text.
    header:         String,
    /// Native header read result.
    header_read:    io::Result<usize>,
    /// Decimal content-length parsing result when the header shape is valid.
    length:         Option<Result<usize, ParseIntError>>,
    /// Complete separator text.
    separator:      String,
    /// Native separator read result.
    separator_read: Option<io::Result<usize>>,
    /// Complete or partially read body bytes.
    body:           Vec<u8>,
    /// Native body read result.
    body_read:      Option<io::Result<usize>>,
    /// Native message-decoding result.
    decoded:        Option<Result<rpc::Message, rpc::MessageDecodeError>>,
  }

  /// Concrete failures of the bounded protocol client.
  #[cfg(feature = "lsp")]
  #[derive(Debug, thiserror::Error)]
  enum ClientFailure {
    /// Sending retained the request and failed serialization or I/O result.
    #[error(transparent)]
    Send(#[from] Box<PredicateFailure<SentMessage>>),
    /// Reading retained all framing observations and native failures.
    #[error(transparent)]
    Frame(#[from] Box<PredicateFailure<ResponseFrame>>),
    /// A decoded response failed its protocol contract.
    #[error(transparent)]
    Response(#[from] Box<PredicateFailure<rpc::Message>>),
    /// A protocol barrier did not retain its required messages.
    #[error(transparent)]
    Messages(#[from] Box<PredicateFailure<Vec<rpc::Message>>>),
    /// A decoded response was unexpectedly missing.
    #[error(transparent)]
    MissingMessage(#[from] Box<OptionFailure<rpc::Message>>),
    /// A JSON result channel was unexpectedly missing.
    #[error(transparent)]
    MissingValue(#[from] Box<OptionFailure<serde_json::Value>>),
    /// A selected JSON value failed its native predicate.
    #[error(transparent)]
    Value(#[from] Box<PredicateFailure<serde_json::Value>>),
  }

  /// Native selected-value assertion contract used by protocol requests.
  #[cfg(feature = "lsp")]
  type JsonContract = fn(serde_json::Value, &'static str) -> Result<serde_json::Value, PredicateFailure<serde_json::Value>>;

  /// Interactive framed client retaining every sent frame and received message.
  #[cfg(feature = "lsp")]
  #[derive(Debug)]
  struct BoundedLspClient {
    /// Fixture-owned standard-input producer.
    input:           taplo_test_support::TestInputWriter,
    /// Streaming reader over captured standard output.
    output:          BufReader<taplo_test_support::TestOutputReader>,
    /// Every response and asynchronous message consumed from output.
    observed:        Vec<rpc::Message>,
    /// Messages present at the document-open response barrier.
    opened_messages: Vec<rpc::Message>,
    /// Complete successful outbound observations.
    sent:            Vec<SentMessage>,
    /// Complete successfully decoded input frames.
    frames:          Vec<ResponseFrame>,
    /// Native terminal input shutdown result.
    shutdown:        Option<io::Result<()>>,
  }

  #[cfg(feature = "lsp")]
  impl BoundedLspClient {
    /// Serialize and write a request, retaining both its input and complete native outcomes.
    async fn send(&mut self, request: rpc::Request<serde_json::Value>, context: &'static str) -> Result<(), ClientFailure> {
      let frame = serde_json::to_vec(&request).map(|body| {
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(&body);
        framed
      });
      let written = if let Ok(ref bytes) = frame {
        Some(AsyncWriteExt::write_all(&mut self.input, bytes).await)
      } else {
        None
      };
      let sent = ensure_that(
        SentMessage {
          request,
          frame,
          written,
        },
        context,
        |actual| actual.frame.is_ok() && actual.written.as_ref().is_some_and(Result::is_ok),
      )
      .map_err(Box::new)?;
      self.sent.push(sent);
      Ok(())
    }

    /// Send one typed JSON-RPC request without awaiting its response.
    async fn send_request(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      context: &'static str,
    ) -> Result<(), ClientFailure> {
      self
        .send(
          rpc::Request::new()
            .with_method(method)
            .with_id(Some(rpc::RequestId::Number(id)))
            .with_params(params),
          context,
        )
        .await
    }

    /// Send one typed JSON-RPC notification.
    async fn notify(&mut self, method: &str, params: Option<serde_json::Value>, context: &'static str) -> Result<(), ClientFailure> {
      self
        .send(rpc::Request::new().with_method(method).with_params(params), context)
        .await
    }

    /// Read one frame, preserving every preceding read if a later framing boundary fails.
    async fn frame(&mut self) -> Result<ResponseFrame, Box<PredicateFailure<ResponseFrame>>> {
      let mut header = String::new();
      let header_read = AsyncBufReadExt::read_line(&mut self.output, &mut header).await;
      let length = header
        .strip_prefix("Content-Length: ")
        .and_then(|value| value.strip_suffix("\r\n"))
        .map(str::parse::<usize>);
      let mut frame = ResponseFrame {
        header,
        header_read,
        length,
        separator: String::new(),
        separator_read: None,
        body: Vec::new(),
        body_read: None,
        decoded: None,
      };
      let byte_count = frame.length.as_ref().and_then(|result| result.as_ref().ok()).copied();
      if byte_count.is_some() {
        frame.separator_read = Some(AsyncBufReadExt::read_line(&mut self.output, &mut frame.separator).await);
      }
      if let Some(count) = byte_count.filter(|_| matches!(frame.separator_read, Some(Ok(2))) && frame.separator == "\r\n") {
        frame.body.resize(count, 0);
        frame.body_read = Some(AsyncReadExt::read_exact(&mut self.output, &mut frame.body).await);
      }
      if frame
        .body_read
        .as_ref()
        .is_some_and(|read| read.as_ref().is_ok_and(|count| *count == frame.body.len()))
      {
        frame.decoded = Some(rpc::decode_slice(&frame.body));
      }
      ensure_that(
        frame,
        "response framing must retain a nonempty standard header, decimal length, exact CRLF separator, complete body, and typed JSON-RPC \
         message",
        |actual| {
          actual
            .header_read
            .as_ref()
            .is_ok_and(|count| *count > 0 && *count == actual.header.len())
            && actual.length.as_ref().is_some_and(Result::is_ok)
            && matches!(&actual.separator_read, Some(Ok(2)))
            && actual.separator == "\r\n"
            && actual
              .body_read
              .as_ref()
              .is_some_and(|read| read.as_ref().is_ok_and(|count| *count == actual.body.len()))
            && actual.decoded.as_ref().is_some_and(Result::is_ok)
        },
      )
      .map_err(Box::new)
    }

    /// Read framed output until the correlated response arrives, retaining all intervening
    /// messages.
    async fn response(&mut self, request_id: i32) -> Result<rpc::Message, ClientFailure> {
      let mut correlated = None;
      while correlated.is_none() {
        let frame = self.frame().await?;
        let message = frame.decoded.as_ref().and_then(|result| result.as_ref().ok()).cloned();
        self.frames.push(frame);
        let decoded_message = ensure_some(message, "a validated response frame must retain its decoded message").map_err(Box::new)?;
        let requested = decoded_message.id == rpc::MessageId::Value(rpc::RequestId::Number(request_id));
        self.observed.push(decoded_message.clone());
        correlated = requested.then_some(decoded_message);
      }
      Ok(ensure_some(correlated, "the response loop must retain the correlated native message").map_err(Box::new)?)
    }

    /// Send a request and retain its complete successful response in the transcript.
    async fn request(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      context: &'static str,
    ) -> Result<serde_json::Value, ClientFailure> {
      self.send_request(id, method, params, context).await?;
      let response = ensure_that(self.response(id).await?, context, |message| {
        message.error.is_none() && message.result.is_some()
      })
      .map_err(Box::new)?;
      Ok(ensure_some(response.result, context).map_err(Box::new)?)
    }

    /// Select JSON from a response whose complete native message remains in the transcript.
    async fn request_selected(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      pointer: &str,
      context: &'static str,
    ) -> Result<serde_json::Value, ClientFailure> {
      let result = self.request(id, method, params, context).await?;
      let selected = ensure_that(result, context, |value| value.pointer(pointer).is_some()).map_err(Box::new)?;
      Ok(ensure_some(selected.pointer(pointer).cloned(), context).map_err(Box::new)?)
    }

    /// Check a selected native JSON subject while preserving the full response transcript.
    async fn request_selected_where(
      &mut self,
      id: i32,
      method: &str,
      params: Option<serde_json::Value>,
      pointer: &str,
      context: &'static str,
      validate: JsonContract,
    ) -> Result<serde_json::Value, ClientFailure> {
      Ok(validate(self.request_selected(id, method, params, pointer, context).await?, context).map_err(Box::new)?)
    }

    /// Check converted text while retaining its native JSON subject and full response transcript.
    async fn request_text_containing(
      &mut self,
      id: i32,
      method: &str,
      source_text: &str,
      expected_text: &str,
      context: &'static str,
    ) -> Result<serde_json::Value, ClientFailure> {
      let result = self
        .request_selected(id, method, Some(serde_json::json!({"text": source_text})), "/text", context)
        .await?;
      Ok(
        ensure_that(result, context, |value| {
          value.as_str().is_some_and(|text| text.contains(expected_text))
        })
        .map_err(Box::new)?,
      )
    }
  }

  /// Require a native JSON array without discarding its contents.
  #[cfg(feature = "lsp")]
  fn json_array_contract(
    value: serde_json::Value,
    context: &'static str,
  ) -> Result<serde_json::Value, PredicateFailure<serde_json::Value>> {
    ensure_that(value, context, serde_json::Value::is_array)
  }

  /// Require a nonempty native JSON array without discarding its contents.
  #[cfg(feature = "lsp")]
  fn non_empty_json_array_contract(
    value: serde_json::Value,
    context: &'static str,
  ) -> Result<serde_json::Value, PredicateFailure<serde_json::Value>> {
    ensure_that(value, context, |subject| {
      subject.as_array().is_some_and(|values| !values.is_empty())
    })
  }

  /// Require protocol null while returning its native JSON value.
  #[cfg(feature = "lsp")]
  fn json_null_contract(value: serde_json::Value, context: &'static str) -> Result<serde_json::Value, PredicateFailure<serde_json::Value>> {
    ensure_that(value, context, serde_json::Value::is_null)
  }

  /// Initialized CLI and its deterministic environment owner.
  type InitializedCli = (TestEnvironment, Taplo<TestEnvironment>);

  /// Native host, CLI initialization, and invalid configuration load observations.
  type ConfigurationFailure = (
    TestEnvironment,
    Result<Taplo<TestEnvironment>, CliError>,
    Option<Result<Arc<Config>, CliError>>,
  );

  /// Execute one independent invalid configuration fixture and retain its host and native outcomes.
  fn configuration_failure((path, contents, reject_read, cwd): (Option<&str>, &[u8], bool, bool)) -> ConfigurationFailure {
    let host = TestEnvironment::default();
    if let Some(target) = path {
      host.insert_file(target, contents.to_vec());
    }
    host.set_read_failure(reject_read);
    if !cwd {
      host.set_cwd(None);
    }
    let mut cli = Taplo::new(host.clone());
    let loaded = cli
      .as_mut()
      .ok()
      .map(|command| drive(command.load_config(&config_general(path, true))));
    (host, cli, loaded)
  }

  /// Construct initialized CLI state and retain its observable host.
  fn initialized_cli() -> Result<InitializedCli, Box<CliError>> {
    let environment = TestEnvironment::default();
    let taplo = Taplo::new(environment.clone()).map_err(Box::new)?;
    Ok((environment, taplo))
  }

  /// Complete command outcome and its channel and filesystem effects.
  #[derive(Debug)]
  struct CommandObservation {
    /// Native CLI completion result.
    result: Result<(), CliError>,
    /// Complete standard-output bytes at completion.
    stdout: Vec<u8>,
    /// Complete diagnostic bytes at completion.
    stderr: Vec<u8>,
    /// Ordered filesystem writes at completion.
    writes: Vec<PathBuf>,
  }

  /// Capture a completed command without projecting its native result or effects.
  fn observe(environment: &TestEnvironment, result: Result<(), CliError>) -> CommandObservation {
    CommandObservation {
      result,
      stdout: environment.stdout(),
      stderr: environment.stderr(),
      writes: environment.writes(),
    }
  }

  /// Execute one query after selecting input and clearing prior output.
  fn get_output(environment: &TestEnvironment, taplo: &Taplo<TestEnvironment>, source: &[u8], command: GetCommand) -> CommandObservation {
    environment.clear_output();
    environment.set_stdin(source.to_vec());
    observe(environment, drive(taplo.execute_get(command)))
  }

  /// Inspect a diagnostic substring while the owning observation retains every byte.
  fn diagnostic_contains(observed: &CommandObservation, text: &str) -> bool {
    from_utf8(&observed.stderr).is_ok_and(|diagnostics| diagnostics.contains(text))
  }

  /// Install the shared schema requiring one string-valued name property.
  #[cfg(feature = "lint")]
  fn install_required_string_schema(environment: &TestEnvironment) {
    environment.insert_file(
      "/workspace/schema.json",
      br#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#.to_vec(),
    );
  }

  /// Parse a test fixture URL through the CLI's native URL error boundary.
  #[cfg(feature = "lint")]
  fn fixture_url(input: &str) -> Result<url::Url, Box<CliError>> {
    url::Url::parse(input).map_err(|source| {
      Box::new(CliError::Url {
        input: input.to_owned(),
        source,
      })
    })
  }

  #[test]
  fn local_dispatcher_executes_local_commands() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let command = observe(
        &environment,
        drive(taplo.execute_local(arguments(TaploCommand::Config {
          cmd: ConfigCommand::Default,
        }))),
      );
      let decoded = from_utf8(&command.stdout).map(toml::from_str::<Config>);
      Ok::<_, Box<CliError>>((taplo, command, decoded))
    })();
    ensure_that(
      observed,
      "local configuration dispatch must preserve the public default configuration",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let Ok(Ok(ref config)) = actual.2 else {
          return false;
        };
        actual.1.result.is_ok()
          && config.include.is_none()
          && config.exclude.is_none()
          && config.rule.is_empty()
          && config.plugins.is_none()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn configuration_commands_emit_both_documents_and_propagate_output_failure() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, taplo) = initialized_cli()?;
      let defaults = observe(&environment, drive(taplo.execute_config(ConfigCommand::Default)));
      environment.clear_output();
      let schema = observe(&environment, drive(taplo.execute_config(ConfigCommand::Schema)));
      let decoded = serde_json::from_slice::<serde_json::Value>(&schema.stdout);
      environment.set_stdout_failure(true);
      let rejected = observe(&environment, drive(taplo.execute_config(ConfigCommand::Default)));
      Ok::<_, Box<CliError>>((taplo, defaults, schema, decoded, rejected))
    })();
    ensure_that(
      observed,
      "configuration commands must emit default and schema documents and retain native stream failures",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.result.is_ok()
          && actual.1.stdout.is_empty()
          && actual.2.result.is_ok()
          && actual.3.as_ref().is_ok_and(|schema| {
            schema.get("title") == Some(&serde_json::json!("Config"))
              && schema.pointer("/properties/include").is_some()
              && schema.pointer("/properties/formatting").is_some()
          })
          && matches!(&actual.4.result, Err(CliError::Io(error)) if error.kind() == ErrorKind::PermissionDenied)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn configuration_loading_discovers_prepares_caches_and_preserves_typed_failures() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      environment.insert_file("/workspace/taplo.toml", b"include = [\"configured/*.toml\"]\n".to_vec());
      environment.insert_file("/workspace/configured/value.toml", b"value=1\n".to_vec());
      environment.insert_file("/workspace/ignored.toml", b"ignored=1\n".to_vec());
      let automatic = config_general(None, false);
      let discovered = drive(taplo.load_config(&automatic));
      let bases = environment.discovery_bases();
      let collected = discovered.as_ref().ok().map(|config| {
        (
          drive(
            taplo.collect_files(
              Path::new("/workspace"),
              config,
              [
                "/workspace/configured/*.toml", "configured/*.toml", "/workspace/configured/*.toml",
              ]
              .into_iter()
              .map(str::to_owned),
            ),
          ),
          drive(taplo.collect_files(Path::new("/workspace"), config, once(String::from("[")))),
        )
      });
      environment.insert_file("/workspace/taplo.toml", b"include = [\"replacement/*.toml\"]\n".to_vec());
      let cached = drive(taplo.load_config(&automatic));
      let cached_bases = environment.discovery_bases();
      let failures = [
        (Some("/config/invalid.toml"), b"include = [".as_slice(), false, true),
        (Some("/config/non-utf8.toml"), &[0xff], false, true),
        (Some("/config/unreadable.toml"), &[], true, true),
        (None, &[], false, false),
      ]
      .map(configuration_failure);
      Ok::<_, Box<CliError>>((taplo, discovered, bases, collected, cached, cached_bases, failures))
    })();
    ensure_that(observed, "configuration discovery must prepare and cache one immutable invocation config while preserving decode, Unicode, read, and cwd failures", |result| {
      result.as_ref().is_ok_and(|actual| actual.1.as_ref().is_ok_and(|config| config.is_included(Path::new("/workspace/configured/value.toml")) && !config.is_included(Path::new("/workspace/ignored.toml"))
        && actual.4.as_ref().is_ok_and(|cached| Arc::ptr_eq(config, cached) && cached.is_included(Path::new("/workspace/configured/value.toml")) && !cached.is_included(Path::new("/workspace/replacement/value.toml"))))
        && actual.2 == [PathBuf::from("/workspace")] && actual.5 == actual.2
        && actual.3.as_ref().is_some_and(|files| files.0.as_ref().is_ok_and(|paths| paths == &[PathBuf::from("/workspace/configured/value.toml")]) && matches!(&files.1, Err(CliError::Glob { pattern, .. }) if pattern == "/workspace/["))
        && actual.6.as_slice().first_chunk::<4>().is_some_and(|failures| {
          let [ref toml, ref utf8, ref read, ref cwd] = *failures;
          matches!(&toml.2, Some(Err(CliError::ConfigDecode { path, .. })) if path == Path::new("/config/invalid.toml"))
            && matches!(&utf8.2, Some(Err(CliError::Utf8(_))))
            && matches!(&read.2, Some(Err(CliError::Environment(EnvironmentError::Io { operation: "read_file", source, .. }))) if source.kind() == ErrorKind::PermissionDenied)
            && matches!(&cwd.2, Some(Err(CliError::Failure(CliFailure::WorkingDirectoryRequired))))
        }))
    }).map(drop).map_err(Box::new)
  }

  #[test]
  fn stdin_formatting_writes_exact_output_and_enforces_check_mode() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let formatted = observe(
        &environment,
        drive(taplo.execute_format(stdin_format_command(&environment, b"value=1\n"))),
      );
      environment.clear_output();
      let mut check = stdin_format_command(&environment, b"value = 1\n");
      check.output.check = true;
      let accepted = observe(&environment, drive(taplo.execute_format(check.clone())));
      environment.set_stdin(b"value=1\n".to_vec());
      let mismatch = observe(&environment, drive(taplo.execute_format(check)));
      environment.clear_output();
      let mut aligned = stdin_format_command(&environment, b"short=1\nlonger=2\n");
      aligned.options.push(String::from("align_entries=true"));
      let overridden = observe(&environment, drive(taplo.execute_format(aligned)));
      Ok::<_, Box<CliError>>((taplo, formatted, accepted, mismatch, overridden))
    })();
    ensure_that(
      observed,
      "stdin formatting must preserve exact output, silent check success, typed mismatch, and command-line overrides",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.result.is_ok()
          && actual.1.stdout == b"value = 1\n"
          && actual.2.result.is_ok()
          && actual.2.stdout.is_empty()
          && matches!(&actual.3.result, Err(CliError::Failure(CliFailure::FormattingMismatch)))
          && actual.4.result.is_ok()
          && actual.4.stdout == b"short  = 1\nlonger = 2\n"
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn stdin_formatting_reports_malformed_input_and_supports_explicit_force() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let blocked = observe(
        &environment,
        drive(taplo.execute_format(stdin_format_command(&environment, b"value =\n"))),
      );
      environment.clear_output();
      let mut forced = stdin_format_command(&environment, b"value =\n");
      forced.input.force = true;
      let accepted = observe(&environment, drive(taplo.execute_format(forced)));
      Ok::<_, Box<CliError>>((taplo, blocked, accepted))
    })();
    ensure_that(
      observed,
      "malformed stdin must be blocked by default and explicit force must preserve both its source span and diagnostic",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        matches!(&actual.1.result, Err(CliError::Failure(CliFailure::FormattingBlocked)))
          && actual.1.stdout.is_empty()
          && diagnostic_contains(&actual.1, "invalid TOML")
          && actual.2.result.is_ok()
          && actual.2.stdout == b"value =\n"
          && diagnostic_contains(&actual.2, "invalid TOML")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn file_formatting_updates_only_changed_files_and_check_diff_never_writes() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let (check_environment, mut check_taplo) = initialized_cli()?;
      environment.insert_file("/workspace/changed.toml", b"value=1\n".to_vec());
      environment.insert_file("/workspace/stable.toml", b"stable = true\n".to_vec());
      let formatted = observe(
        &environment,
        drive(taplo.execute_format(format_command(Vec::from([String::from("*.toml")])))),
      );
      let files = [
        environment.read_file(Path::new("/workspace/changed.toml")),
        environment.read_file(Path::new("/workspace/stable.toml")),
      ];
      check_environment.insert_file("/workspace/input.toml", b"value=1\n".to_vec());
      let mut check = format_command(Vec::from([String::from("input.toml")]));
      check.output.check = true;
      check.output.diff = true;
      let checked = observe(&check_environment, drive(check_taplo.execute_format(check)));
      Ok::<_, Box<CliError>>((taplo, check_taplo, formatted, files, checked))
    })();
    ensure_that(
      observed,
      "file formatting must write only changed documents and check-plus-diff must retain edits without writing",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref changed, ref stable] = actual.3;
        actual.2.result.is_ok()
          && changed.as_ref().is_ok_and(|bytes| bytes == b"value = 1\n")
          && stable.as_ref().is_ok_and(|bytes| bytes == b"stable = true\n")
          && actual.2.writes == [PathBuf::from("/workspace/changed.toml")]
          && matches!(&actual.4.result, Err(CliError::Failure(CliFailure::FileFormattingFailed)))
          && actual.4.writes.is_empty()
          && from_utf8(&actual.4.stdout).is_ok_and(|diff| ["diff a/", "-value=1", "+value = 1"].iter().all(|part| diff.contains(part)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn file_formatting_aggregates_malformed_inputs_and_force_preserves_diagnostics() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let (forced_environment, mut forced_taplo) = initialized_cli()?;
      environment.insert_file("/workspace/broken.toml", b"value=\n".to_vec());
      environment.insert_file("/workspace/changed.toml", b"changed=1\n".to_vec());
      let mut command = format_command(Vec::from([String::from("*.toml")]));
      command.stdin_filepath = Some(String::from("/ignored/stdin.toml"));
      let blocked = observe(&environment, drive(taplo.execute_format(command)));
      let files = [
        environment.read_file(Path::new("/workspace/broken.toml")),
        environment.read_file(Path::new("/workspace/changed.toml")),
      ];
      forced_environment.insert_file("/workspace/broken.toml", b"value=\n".to_vec());
      let mut forced_command = format_command(Vec::from([String::from("broken.toml")]));
      forced_command.input.force = true;
      let forced = observe(&forced_environment, drive(forced_taplo.execute_format(forced_command)));
      let preserved = forced_environment.read_file(Path::new("/workspace/broken.toml"));
      Ok::<_, Box<CliError>>((taplo, forced_taplo, blocked, files, forced, preserved))
    })();
    ensure_that(
      observed,
      "file formatting must preserve malformed sources, format valid siblings, and retain source-specific diagnostics under force",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref broken, ref changed] = actual.3;
        matches!(&actual.2.result, Err(CliError::Failure(CliFailure::FileFormattingFailed)))
          && broken.as_ref().is_ok_and(|bytes| bytes == b"value=\n")
          && changed.as_ref().is_ok_and(|bytes| bytes == b"changed = 1\n")
          && diagnostic_contains(&actual.2, "/workspace/broken.toml")
          && !diagnostic_contains(&actual.2, "/ignored/stdin.toml")
          && actual.4.result.is_ok()
          && actual.4.writes.is_empty()
          && diagnostic_contains(&actual.4, "invalid TOML")
          && actual.5.as_ref().is_ok_and(|bytes| bytes == b"value=\n")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn formatting_validates_input_identity_options_and_host_failures() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let (missing_environment, mut missing_taplo) = initialized_cli()?;
      let (output_environment, mut output_taplo) = initialized_cli()?;
      let identities = ["nested/input.toml", "/virtual/input.toml"].map(|path| {
        environment.clear_output();
        let mut command = stdin_format_command(&environment, b"value =\n");
        command.stdin_filepath = Some(path.to_owned());
        observe(&environment, drive(taplo.execute_format(command)))
      });
      missing_environment.set_cwd(None);
      let mut relative = stdin_format_command(&missing_environment, b"value = 1\n");
      relative.stdin_filepath = Some(String::from("relative.toml"));
      let unresolved = observe(&missing_environment, drive(missing_taplo.execute_format(relative)));
      let options = ["not-an-assignment", "align_entries=not-a-bool"].map(|option| {
        let mut command = stdin_format_command(&environment, b"value=1\n");
        command.options.push(option.to_owned());
        observe(&environment, drive(taplo.execute_format(command)))
      });
      let no_cwd = observe(
        &missing_environment,
        drive(missing_taplo.execute_format(format_command(Vec::new()))),
      );
      let command = stdin_format_command(&output_environment, b"value=1\n");
      output_environment.set_stdout_failure(true);
      let output_failure = observe(&output_environment, drive(output_taplo.execute_format(command)));
      Ok::<_, Box<CliError>>((
        taplo, missing_taplo, output_taplo, identities, unresolved, options, no_cwd, output_failure,
      ))
    })();
    ensure_that(observed, "formatting must retain input identities, typed option failures, required cwd, and output I/O failures", |result| {
      let Ok(ref actual) = *result else { return false; };
        let [ref relative, ref absolute] = actual.3;
        let [ref option, ref value] = actual.5;
        actual.3.iter().all(|command| matches!(&command.result, Err(CliError::Failure(CliFailure::FormattingBlocked))))
          && diagnostic_contains(relative, "/workspace/nested/input.toml") && diagnostic_contains(absolute, "/virtual/input.toml") && !diagnostic_contains(absolute, "/workspace/virtual/input.toml")
          && [&actual.4, &actual.6].iter().all(|command| matches!(&command.result, Err(CliError::Failure(CliFailure::WorkingDirectoryRequired))))
          && matches!(&option.result, Err(CliError::FormatOption(OptionParseError::InvalidOption(input))) if input == "not-an-assignment")
          && matches!(&value.result, Err(CliError::FormatOption(OptionParseError::InvalidValue { key, input, expected, .. })) if key == "align_entries" && input == "not-a-bool" && *expected == "bool")
          && matches!(&actual.7.result, Err(CliError::Io(error)) if error.kind() == ErrorKind::PermissionDenied)

    }).map(drop).map_err(Box::new)
  }

  #[test]
  fn queries_render_value_json_and_toml_contracts() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, taplo) = initialized_cli()?;
      let source = b"name = \"taplo\"\nvalues = [1, 2]\n[table]\nkey = \"value\"\n";
      let scalar = get_output(&environment, &taplo, source, get_command(Some("name")));
      let mut array_command = get_command(Some("values"));
      array_command.separator = Some(String::from(","));
      let array = get_output(&environment, &taplo, source, array_command);
      let mut json_command = formatted_get_command(Some("table"), OutputFormat::Json);
      json_command.strip_newline = true;
      let json = get_output(&environment, &taplo, source, json_command);
      let decoded = serde_json::from_slice::<serde_json::Value>(&json.stdout);
      let toml = get_output(
        &environment,
        &taplo,
        source,
        formatted_get_command(Some("table"), OutputFormat::Toml),
      );
      Ok::<_, Box<CliError>>((taplo, scalar, array, json, decoded, toml))
    })();
    ensure_that(
      observed,
      "value, JSON, and TOML queries must preserve scalar, array-order, and complete-table output contracts",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.result.is_ok()
          && actual.1.stdout == b"taplo\n"
          && actual.2.result.is_ok()
          && actual.2.stdout == b"1,2\n"
          && actual.3.result.is_ok()
          && actual.4.as_ref().is_ok_and(|json| json == &serde_json::json!({"key": "value"}))
          && actual.5.result.is_ok()
          && actual.5.stdout == b"key = \"value\"\n"
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn queries_render_whole_documents_multi_matches_and_scalar_families() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, taplo) = initialized_cli()?;
      let source = b"item_a = 1\nitem_b = 2\nother = 3\n";
      let whole = get_output(&environment, &taplo, source, formatted_get_command(None, OutputFormat::Json));
      let whole_json = serde_json::from_slice::<serde_json::Value>(&whole.stdout);
      let multi = get_output(
        &environment,
        &taplo,
        source,
        formatted_get_command(Some("item_*"), OutputFormat::Json),
      );
      let multi_json = serde_json::from_slice::<serde_json::Value>(&multi.stdout);
      let toml = get_output(
        &environment,
        &taplo,
        source,
        formatted_get_command(Some("item_*"), OutputFormat::Toml),
      );
      let scalars = [
        (".enabled", "true\n"),
        ("ratio", "1.5\n"),
        ("timestamp", "1979-05-27T07:32:00Z\n"),
      ]
      .map(|(pattern, expected)| {
        (
          get_output(
            &environment,
            &taplo,
            b"enabled = true\nratio = 1.5\ntimestamp = 1979-05-27T07:32:00Z\n",
            get_command(Some(pattern)),
          ),
          expected,
        )
      });
      let mut strip = formatted_get_command(None, OutputFormat::Toml);
      strip.strip_newline = true;
      let stripped = get_output(&environment, &taplo, b"value = 1\n", strip);
      Ok::<_, Box<CliError>>((taplo, whole, whole_json, multi, multi_json, toml, scalars, stripped))
    })();
    ensure_that(
      observed,
      "whole-document and multi-match queries must retain envelopes, scalar families, order, and newline policy",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.result.is_ok()
          && actual.1.stdout.ends_with(b"\n")
          && actual
            .2
            .as_ref()
            .is_ok_and(|json| json == &serde_json::json!({"item_a": 1, "item_b": 2, "other": 3}))
          && actual.3.result.is_ok()
          && actual.4.as_ref().is_ok_and(|json| json == &serde_json::json!([1, 2]))
          && actual.5.result.is_ok()
          && actual.5.stdout == b"[\n  1,\n  2,\n]\n"
          && actual
            .6
            .iter()
            .all(|entry| entry.0.result.is_ok() && entry.0.stdout == entry.1.as_bytes())
          && actual.7.result.is_ok()
          && actual.7.stdout == b"value = 1"
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn queries_reject_incompatible_missing_table_and_invalid_documents() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, taplo) = initialized_cli()?;
      let mut separator_command = formatted_get_command(Some("value"), OutputFormat::Json);
      separator_command.separator = Some(String::from(","));
      let separator = get_output(&environment, &taplo, b"value = 1\n", separator_command);
      let cases = [
        ("value = 1\n", Some("missing"), CliFailure::NoQueryMatches, None),
        ("[table]\nvalue = 1\n", Some("table"), CliFailure::TableValueOutput, None),
        ("value = 1\n", None, CliFailure::TableValueOutput, None),
        ("value =\n", Some("value"), CliFailure::SyntaxErrors, Some("invalid TOML")),
        (
          "value = 1\nvalue = 2\n",
          Some("value"),
          CliFailure::SemanticErrors,
          Some("conflicting keys"),
        ),
      ]
      .map(|(source, pattern, expected, diagnostic)| {
        (
          get_output(&environment, &taplo, source.as_bytes(), get_command(pattern)),
          expected,
          diagnostic,
        )
      });
      Ok::<_, Box<CliError>>((taplo, separator, cases))
    })();
    ensure_that(
      observed,
      "queries must retain distinct separator, absence, table, syntax, and semantic failures with required diagnostics",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        matches!(&actual.1.result, Err(CliError::Failure(CliFailure::InvalidSeparator)))
          && actual.2.iter().all(|case| {
            matches!(&case.0.result, Err(CliError::Failure(failure)) if *failure == case.1)
              && case.2.is_none_or(|text| diagnostic_contains(&case.0, text))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn queries_read_files_strip_newlines_and_propagate_output_failures() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, taplo) = initialized_cli()?;
      environment.insert_file("/workspace/query.toml", b"value = \"file\"\n".to_vec());
      let mut command = get_command(Some("value"));
      command.file_path = Some(PathBuf::from("/workspace/query.toml"));
      command.strip_newline = true;
      let file = observe(&environment, drive(taplo.execute_get(command)));
      environment.set_stdout_failure(true);
      let rejected = get_output(&environment, &taplo, b"value = 1\n", get_command(Some("value")));
      Ok::<_, Box<CliError>>((taplo, file, rejected))
    })();
    ensure_that(
      observed,
      "file queries must honor newline stripping and propagate output failures without fabricating syntax diagnostics",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.result.is_ok()
          && actual.1.stdout == b"file"
          && matches!(&actual.2.result, Err(CliError::Io(error)) if error.kind() == ErrorKind::PermissionDenied)
          && !diagnostic_contains(&actual.2, "invalid TOML")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "toml-test")]
  #[test]
  fn toml_test_serializes_scalar_families_and_nested_containers() -> Result<(), impl Debug> {
    let observed = (|| {
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
      let command = observe(&environment, drive(taplo.execute_toml_test()));
      let decoded = serde_json::from_slice::<serde_json::Value>(&command.stdout);
      Ok::<_, Box<CliError>>((taplo, command, decoded))
    })();
    ensure_that(
      observed,
      "TOML-test output must preserve scalar labels, canonical values, and recursive container envelopes",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.result.is_ok()
          && actual.2.as_ref().is_ok_and(|json| {
            [
              ("/string/type", "string"),
              ("/integer/value", "7"),
              ("/offset/type", "datetime"),
              ("/local_datetime/type", "datetime-local"),
              ("/local_date/type", "date-local"),
              ("/local_time/type", "time-local"),
              ("/array/1/value", "two"),
            ]
            .iter()
            .all(|&(pointer, expected)| json.pointer(pointer).and_then(serde_json::Value::as_str) == Some(expected))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "toml-test")]
  #[test]
  fn toml_test_rejects_syntax_semantics_and_output_failures() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, taplo) = initialized_cli()?;
      let invalid = [b"value =\n".as_slice(), b"value = 1\nvalue = 2\n".as_slice()].map(|source| {
        environment.clear_output();
        environment.set_stdin(source.to_vec());
        observe(&environment, drive(taplo.execute_toml_test()))
      });
      environment.clear_output();
      environment.set_stdin(b"value = 1\n".to_vec());
      environment.set_stdout_failure(true);
      let output = observe(&environment, drive(taplo.execute_toml_test()));
      Ok::<_, Box<CliError>>((taplo, invalid, output))
    })();
    ensure_that(
      observed,
      "TOML-test must reject syntax and semantic failures with diagnostics and retain output I/O failures",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref syntax, ref semantic] = actual.1;
        actual
          .1
          .iter()
          .all(|command| matches!(&command.result, Err(CliError::Failure(CliFailure::InvalidTomlTestInput))))
          && !syntax.stderr.is_empty()
          && diagnostic_contains(semantic, "conflicting keys")
          && matches!(&actual.2.result, Err(CliError::Io(error)) if error.kind() == ErrorKind::PermissionDenied)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "lint")]
  #[test]
  fn lint_accepts_clean_input_and_reports_syntax_and_semantic_failures() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let commands = [
        b"value = 1\n".as_slice(),
        b"value =\n".as_slice(),
        b"value = 1\nvalue = 2\n".as_slice(),
      ]
      .map(|source| {
        environment.clear_output();
        environment.set_stdin(source.to_vec());
        observe(&environment, drive(taplo.execute_lint(lint_command())))
      });
      Ok::<_, Box<CliError>>((taplo, commands))
    })();
    ensure_that(
      observed,
      "schema-disabled lint must accept clean input and retain distinct syntax and semantic diagnostics",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref clean, ref syntax, ref semantic] = actual.1;
        clean.result.is_ok()
          && clean.stderr.is_empty()
          && matches!(&syntax.result, Err(CliError::Failure(CliFailure::SyntaxErrors)))
          && diagnostic_contains(syntax, "invalid TOML")
          && matches!(&semantic.result, Err(CliError::Failure(CliFailure::SemanticErrors)))
          && diagnostic_contains(semantic, "conflicting keys")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "lint")]
  #[test]
  fn lint_validates_explicit_schema_and_aggregates_file_failures() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let (file_environment, mut file_taplo) = initialized_cli()?;
      let mut schema = lint_command();
      schema.no_schema = false;
      schema.schema = Some(fixture_url("file:///workspace/schema.json")?);
      install_required_string_schema(&environment);
      let checked = [b"name = 7\n".as_slice(), b"name = \"taplo\"\n".as_slice()].map(|source| {
        environment.clear_output();
        environment.set_stdin(source.to_vec());
        observe(&environment, drive(taplo.execute_lint(schema.clone())))
      });
      file_environment.insert_file("/workspace/invalid.toml", b"value =\n".to_vec());
      let mut files = lint_command();
      files.files = Vec::from([String::from("invalid.toml")]);
      let file = observe(&file_environment, drive(file_taplo.execute_lint(files)));
      Ok::<_, Box<CliError>>((taplo, file_taplo, checked, file))
    })();
    ensure_that(
      observed,
      "explicit schema lint must preserve both validation polarities and aggregate file failures with source diagnostics",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref rejected, ref accepted] = actual.2;
        matches!(&rejected.result, Err(CliError::Failure(CliFailure::SchemaValidation)))
          && !rejected.stderr.is_empty()
          && accepted.result.is_ok()
          && accepted.stderr.is_empty()
          && matches!(&actual.3.result, Err(CliError::Failure(CliFailure::FileValidationFailed)))
          && diagnostic_contains(&actual.3, "invalid TOML")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "lint")]
  #[test]
  fn lint_file_mode_and_schema_configuration_preserve_host_and_policy_boundaries() -> Result<(), impl Debug> {
    let observed = (|| {
      let (file_environment, mut file_taplo) = initialized_cli()?;
      let (missing_cwd, mut missing_taplo) = initialized_cli()?;
      let (invalid_utf8, mut invalid_taplo) = initialized_cli()?;
      let (disabled_environment, mut disabled_taplo) = initialized_cli()?;
      let (catalog_environment, mut catalog_taplo) = initialized_cli()?;
      let catalog_url = fixture_url("file:///workspace/catalog.json")?;
      file_environment.insert_file("/workspace/valid.toml", b"value = 1\n".to_vec());
      let mut valid_file = lint_command();
      valid_file.files = Vec::from([String::from("valid.toml")]);
      let valid = observe(&file_environment, drive(file_taplo.execute_lint(valid_file)));
      missing_cwd.set_cwd(None);
      let mut unresolved = lint_command();
      unresolved.files = Vec::from([String::from("relative.toml")]);
      let missing = observe(&missing_cwd, drive(missing_taplo.execute_lint(unresolved)));
      invalid_utf8.insert_file("/workspace/non-utf8.toml", vec![0xff]);
      let mut invalid_file = lint_command();
      invalid_file.files = Vec::from([String::from("non-utf8.toml")]);
      let invalid = observe(&invalid_utf8, drive(invalid_taplo.execute_lint(invalid_file)));
      install_required_string_schema(&disabled_environment);
      disabled_environment.insert_file(
        "/workspace/disabled-schema.toml",
        b"[schema]\nenabled = false\npath = \"/workspace/schema.json\"\n".to_vec(),
      );
      disabled_environment.set_stdin(b"name = 7\n".to_vec());
      let mut disabled_command = lint_command();
      disabled_command.general = config_general(Some("/workspace/disabled-schema.toml"), true);
      disabled_command.no_schema = false;
      let disabled = observe(&disabled_environment, drive(disabled_taplo.execute_lint(disabled_command)));
      install_required_string_schema(&catalog_environment);
      catalog_environment.insert_file("/workspace/catalog.json", br#"{"schemas":[{"title":"fixture","description":"","url":"file:///workspace/schema.json","urlHash":"","authors":[],"version":null,"patterns":[".*\\.toml$"]}]}"#.to_vec());
      let mut catalog = lint_command();
      catalog.no_schema = false;
      catalog.schema_catalog = Vec::from([catalog_url]);
      let catalogs = [b"name = 7\n".as_slice(), b"name = \"taplo\"\n".as_slice()].map(|source| {
        catalog_environment.clear_output();
        catalog_environment.set_stdin(source.to_vec());
        observe(&catalog_environment, drive(catalog_taplo.execute_lint(catalog.clone())))
      });
      Ok::<_, Box<CliError>>((
        (file_taplo, missing_taplo, invalid_taplo, disabled_taplo, catalog_taplo),
        valid,
        missing,
        invalid,
        disabled,
        catalogs,
      ))
    })();
    ensure_that(
      observed,
      "file lint, disabled schemas, and selected catalogs must preserve host and policy boundaries",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        let [ref catalog_rejected, ref catalog_accepted] = actual.5;
        actual.1.result.is_ok()
          && actual.1.stderr.is_empty()
          && matches!(&actual.2.result, Err(CliError::Failure(CliFailure::WorkingDirectoryRequired)))
          && matches!(&actual.3.result, Err(CliError::Failure(CliFailure::FileValidationFailed)))
          && actual.4.result.is_ok()
          && actual.4.stderr.is_empty()
          && matches!(&catalog_rejected.result, Err(CliError::Failure(CliFailure::SchemaValidation)))
          && catalog_accepted.result.is_ok()
          && catalog_accepted.stderr.is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn dispatcher_applies_all_color_policies() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let commands = [Colors::Auto, Colors::Always, Colors::Never].map(|colors| {
        environment.clear_output();
        let mut command = arguments(TaploCommand::Config {
          cmd: ConfigCommand::Default,
        });
        command.colors = colors;
        (observe(&environment, drive(taplo.execute_local(command))), taplo.colors)
      });
      Ok::<_, Box<CliError>>((taplo, commands))
    })();
    ensure_that(
      observed,
      "the dispatcher must select automatic, always, and never color policies",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.1.iter().all(|entry| entry.0.result.is_ok()) && actual.1.each_ref().map(|entry| entry.1) == [false, true, false]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "completions")]
  #[test]
  fn dispatcher_rejects_an_unknown_completion_shell() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let command = observe(
        &environment,
        drive(taplo.execute_local(arguments(TaploCommand::Completions {
          shell: String::from("unknown-shell"),
        }))),
      );
      Ok::<_, Box<CliError>>((taplo, command))
    })();
    ensure_that(
      observed,
      "completion dispatch must retain its rejected shell in the typed error",
      |result| {
        result
          .as_ref()
          .is_ok_and(|actual| matches!(&actual.1.result, Err(CliError::InvalidShell { shell, .. }) if shell == "unknown-shell"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "lsp")]
  #[test]
  fn local_dispatcher_rejects_concurrent_command() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let command = observe(
        &environment,
        drive(taplo.execute_local(arguments(TaploCommand::Lsp {
          cmd: stdio_lsp_command()
        }))),
      );
      Ok::<_, Box<CliError>>((taplo, command))
    })();
    ensure_that(
      observed,
      "local dispatch must retain the concurrent-capability boundary",
      |result| {
        result
          .as_ref()
          .is_ok_and(|actual| matches!(&actual.1.result, Err(CliError::Failure(CliFailure::ConcurrentEnvironmentRequired))))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Successful native checks returned by each phase of the bounded protocol transaction.
  #[cfg(feature = "lsp")]
  type ProtocolChecks = (
    (rpc::Message, Vec<serde_json::Value>),
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
    (serde_json::Value, serde_json::Value),
    Vec<serde_json::Value>,
  );

  /// Native completion state of both peers; an unpolled completion remains absent on failure.
  #[cfg(feature = "lsp")]
  type ProtocolExecution = (Option<Result<(), CliError>>, Option<Result<ProtocolChecks, ClientFailure>>);

  /// Reject ordinary work before initialization, then install deterministic schema configuration.
  #[cfg(feature = "lsp")]
  async fn initialize_protocol(
    client: &mut BoundedLspClient,
    document_uri: &str,
    positioned: &serde_json::Value,
  ) -> Result<(rpc::Message, Vec<serde_json::Value>), ClientFailure> {
    client
      .send_request(
        -1,
        "textDocument/hover",
        Some(positioned.clone()),
        "send the pre-initialization request",
      )
      .await?;
    let rejected = ensure_that(
      client.response(-1).await?,
      "reject ordinary work before initialization without a success result",
      |message| message.result.is_none() && message.error.as_ref() == Some(&rpc::RpcError::server_not_initialized()),
    )
    .map_err(Box::new)?;
    let initialized = client
      .request_selected(
        0,
        "initialize",
        Some(serde_json::json!({"processId": null, "rootUri": null, "capabilities": {}, "workspaceFolders": []})),
        "/capabilities/textDocumentSync",
        "advertise full document synchronization",
      )
      .await?;
    client
      .notify(
        "workspace/didChangeConfiguration",
        Some(serde_json::json!({"settings": {"schema": {"catalogs": []}}})),
        "push deterministic configuration",
      )
      .await?;
    let schemas = client
      .request_selected_where(
        1,
        "taplo/listSchemas",
        Some(serde_json::json!({"documentUri": document_uri})),
        "/schemas",
        "observe a schema collection after pushed configuration",
        json_array_contract,
      )
      .await?;
    Ok((rejected, Vec::from([initialized, schemas])))
  }

  /// Open a real document and observe diagnostics, symbols, folding, and formatting at response
  /// barriers.
  #[cfg(feature = "lsp")]
  async fn open_protocol_document(
    client: &mut BoundedLspClient,
    document_uri: &str,
    document: &serde_json::Value,
  ) -> Result<Vec<serde_json::Value>, ClientFailure> {
    client.notify("textDocument/didOpen", Some(serde_json::json!({"textDocument": {"uri": document_uri, "languageId": "toml", "version": 1, "text": "name=\"taplo\"\nvalues = [1, 2]\n"}})), "open the bounded document").await?;
    let folding = client
      .request_selected_where(
        2,
        "textDocument/foldingRange",
        Some(document.clone()),
        "",
        "produce a folding-range collection",
        json_array_contract,
      )
      .await?;
    client.opened_messages = ensure_that(
      client.observed.clone(),
      "opening the document must publish replacement diagnostics",
      |messages| {
        messages
          .iter()
          .any(|message| message.method.as_deref() == Some("textDocument/publishDiagnostics"))
      },
    )
    .map_err(Box::new)?;
    let symbols = client
      .request_selected_where(
        3,
        "textDocument/documentSymbol",
        Some(document.clone()),
        "",
        "expose named symbols",
        non_empty_json_array_contract,
      )
      .await?;
    let edits = client
      .request_selected_where(
        4,
        "textDocument/formatting",
        Some(serde_json::json!({"textDocument": {"uri": document_uri}, "options": {"tabSize": 2, "insertSpaces": true}})),
        "",
        "produce a replacement formatting edit",
        non_empty_json_array_contract,
      )
      .await?;
    Ok(Vec::from([folding, symbols, edits]))
  }

  /// Preserve schema-dependent absence and the registered semantic-token data collection.
  #[cfg(feature = "lsp")]
  async fn inspect_protocol_document(
    client: &mut BoundedLspClient,
    document: &serde_json::Value,
    positioned: &serde_json::Value,
  ) -> Result<Vec<serde_json::Value>, ClientFailure> {
    let mut values = Vec::new();
    for (id, method, params) in [
      (5, "textDocument/completion", positioned),
      (6, "textDocument/hover", positioned),
      (7, "textDocument/documentLink", document),
    ] {
      values.push(
        client
          .request_selected_where(
            id,
            method,
            Some(params.clone()),
            "",
            "schema-dependent output must remain absent",
            json_null_contract,
          )
          .await?,
      );
    }
    values.push(
      client
        .request_selected_where(
          8,
          "textDocument/semanticTokens/full",
          Some(document.clone()),
          "/data",
          "retain semantic-token wire data",
          json_array_contract,
        )
        .await?,
    );
    Ok(values)
  }

  /// Prepare a rename target and preserve the resulting workspace edit.
  #[cfg(feature = "lsp")]
  async fn rename_protocol_document(
    client: &mut BoundedLspClient,
    document_uri: &str,
    positioned: &serde_json::Value,
  ) -> Result<(serde_json::Value, serde_json::Value), ClientFailure> {
    let prepared = client
      .request(
        9,
        "textDocument/prepareRename",
        Some(positioned.clone()),
        "prepare an identifier rename target",
      )
      .await?;
    let target = ensure_that(prepared, "prepare an identifier rename target", |value| !value.is_null()).map_err(Box::new)?;
    let changes = client
      .request_selected(
        10,
        "textDocument/rename",
        Some(serde_json::json!({"textDocument": {"uri": document_uri}, "position": {"line": 0, "character": 1}, "newName": "renamed"})),
        "/changes",
        "return a workspace edit for the selected identifier",
      )
      .await?;
    Ok((target, changes))
  }

  /// Exercise both conversion directions, then shut down and close input after terminal exit.
  #[cfg(feature = "lsp")]
  async fn finish_protocol(client: &mut BoundedLspClient, document_uri: &str) -> Result<Vec<serde_json::Value>, ClientFailure> {
    let mut values = Vec::new();
    for (id, method, source, expected) in [
      (11, "taplo/convertToJson", "name = \"taplo\"\n", "\"name\""),
      (12, "taplo/convertToToml", "{\"name\":\"taplo\"}", "name"),
    ] {
      values.push(
        client
          .request_text_containing(id, method, source, expected, "return converted text")
          .await?,
      );
    }
    values.push(
      client
        .request_selected_where(
          13,
          "taplo/associatedSchema",
          Some(serde_json::json!({"documentUri": document_uri})),
          "/schema",
          "retain no effective schema",
          json_null_contract,
        )
        .await?,
    );
    values.push(
      client
        .request_selected_where(
          99,
          "shutdown",
          None,
          "",
          "emit the standard null shutdown result",
          json_null_contract,
        )
        .await?,
    );
    client.notify("exit", None, "send terminal exit").await?;
    client.shutdown = Some(AsyncWriteExt::shutdown(&mut client.input).await);
    Ok(values)
  }

  /// Drive the bounded protocol phases while the client retains their entire native transcript.
  #[cfg(feature = "lsp")]
  async fn exercise_protocol(client: &mut BoundedLspClient) -> Result<ProtocolChecks, ClientFailure> {
    let document_uri = "file:///workspace/document.toml";
    let document = serde_json::json!({"textDocument": {"uri": document_uri}});
    let positioned = serde_json::json!({"textDocument": {"uri": document_uri}, "position": {"line": 0, "character": 1}});
    let initialized = initialize_protocol(client, document_uri, &positioned).await?;
    let opened = open_protocol_document(client, document_uri, &document).await?;
    let inspected = inspect_protocol_document(client, &document, &positioned).await?;
    let renamed = rename_protocol_document(client, document_uri, &positioned).await?;
    let finished = finish_protocol(client, document_uri).await?;
    Ok((initialized, opened, inspected, renamed, finished))
  }

  /// Poll both peers, retaining partial client state when either side fails before shutdown.
  #[cfg(feature = "lsp")]
  fn drive_protocol<'operation>(
    taplo: &'operation mut Taplo<TestEnvironment>,
    client: &'operation mut BoundedLspClient,
  ) -> LocalCommandFuture<'operation, ProtocolExecution> {
    Box::pin(async move {
      let command = taplo.execute(arguments(TaploCommand::Lsp {
        cmd: stdio_lsp_command()
      }));
      let transaction = exercise_protocol(client);
      tokio::pin!(command);
      tokio::pin!(transaction);
      match select(command, transaction).await {
        Either::Left((Ok(()), pending_client)) => (Some(Ok(())), Some(pending_client.await)),
        Either::Left((Err(error), _)) => (Some(Err(error)), None),
        Either::Right((Ok(checks), pending_command)) => (Some(pending_command.await), Some(Ok(checks))),
        Either::Right((Err(error), _)) => (None, Some(Err(error))),
      }
    })
  }

  #[cfg(feature = "lsp")]
  #[test]
  fn full_dispatcher_completes_a_bounded_protocol_transaction() -> Result<(), impl Debug> {
    let observed = (|| {
      let (environment, mut taplo) = initialized_cli()?;
      let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Box::new(CliError::Io(error)))?;
      let mut client = BoundedLspClient {
        input:           environment.interactive_stdin(),
        output:          BufReader::new(environment.stdout_reader()),
        observed:        Vec::new(),
        opened_messages: Vec::new(),
        sent:            Vec::new(),
        frames:          Vec::new(),
        shutdown:        None,
      };
      let (command_result, client_result) = runtime.block_on(drive_protocol(&mut taplo, &mut client));
      let stderr = environment.stderr();
      Ok::<_, Box<CliError>>((taplo, runtime, command_result, client_result, client, stderr))
    })();

    ensure_that(
      observed,
      "the bounded LSP transaction must complete both peers, retain its protocol transcript, close input after exit, and emit no command \
       diagnostics",
      |result| {
        let Ok(ref actual) = *result else {
          return false;
        };
        actual.2.as_ref().is_some_and(Result::is_ok)
          && actual.3.as_ref().is_some_and(Result::is_ok)
          && actual.4.shutdown.as_ref().is_some_and(Result::is_ok)
          && actual
            .4
            .sent
            .first()
            .is_some_and(|sent| sent.request.method == "textDocument/hover")
          && actual.4.sent.last().is_some_and(|sent| sent.request.method == "exit")
          && actual
            .4
            .opened_messages
            .iter()
            .any(|message| message.method.as_deref() == Some("textDocument/publishDiagnostics"))
          && actual.5.is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
