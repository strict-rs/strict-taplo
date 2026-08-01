//! TOML syntax, semantic, and schema lint command.

use std::path::Path;
use std::sync::Arc;

use codespan_reporting::files::SimpleFile;
use serde_json::Value as JsonValue;
use serde_json::json;
use taplo::parser;
use taplo_common::config::Config;
use taplo_common::environment::LocalEnvironment;
use taplo_common::schema::associations::AssociationError;
use taplo_common::schema::associations::AssociationRule;
use taplo_common::schema::associations::DEFAULT_CATALOGS;
use taplo_common::schema::associations::SchemaAssociation;
use tokio::io::AsyncReadExt as _;
use url::Url;

use crate::CliError;
use crate::CliFailure;
use crate::LocalCommandFuture;
use crate::Taplo;
use crate::args::LintCommand;
use crate::path_text;
use crate::printing::print_parse_errors;
use crate::printing::print_schema_errors;
use crate::printing::print_semantic_errors;

/// Execute one lint command.
pub(super) fn execute_lint<E: LocalEnvironment>(
  taplo: &mut Taplo<E>,
  command: LintCommand,
) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    taplo.schemas.cache().set_cache_path(command.general.cache_path.clone());
    let config = taplo.load_config(&command.general).await?;
    prepare_schema_associations(taplo, &command, &config).await?;

    if matches!(command.files.first().map(String::as_str), Some("-")) {
      lint_stdin(taplo, config).await
    } else {
      lint_files(taplo, command, config).await
    }
  })
}

/// Prepare command-selected schema associations before document validation.
fn prepare_schema_associations<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  command: &'operation LintCommand,
  config: &'operation Config,
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    if command.no_schema {
      return Ok(());
    }

    if let Some(schema_url) = command.schema.clone() {
      taplo.schemas.associations().add(
        AssociationRule::regex(".*").map_err(|source| AssociationError::Regex {
          pattern: ".*".into(),
          name:    "command-line schema".into(),
          source:  Box::new(source),
        })?,
        SchemaAssociation {
          meta:     json!({"source": "command-line"}),
          url:      schema_url,
          priority: 999,
        },
      );
      return Ok(());
    }

    taplo.schemas.associations().add_from_config(config);
    for catalog in &command.schema_catalog {
      taplo.schemas.associations().add_from_catalog(catalog).await?;
    }
    if command.default_schema_catalogs {
      for catalog in DEFAULT_CATALOGS {
        let url = Url::parse(catalog).map_err(|url_error| CliError::Url {
          input:  (*catalog).into(),
          source: url_error,
        })?;
        taplo.schemas.associations().add_from_catalog(&url).await?;
      }
    }
    Ok(())
  })
}

/// Lint standard input.
#[tracing::instrument(skip_all)]
fn lint_stdin<E: LocalEnvironment>(taplo: &Taplo<E>, config: Arc<Config>) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    let mut source = String::new();
    let bytes_read = taplo.env.stdin().read_to_string(&mut source).await?;
    tracing::trace!(bytes_read, "read lint input from standard input");
    lint_source(taplo, Path::new("-"), &source, &config).await
  })
}

/// Lint selected files.
#[tracing::instrument(skip_all)]
fn lint_files<E: LocalEnvironment>(
  taplo: &Taplo<E>,
  command: LintCommand,
  config: Arc<Config>,
) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    let cwd = taplo.env.cwd_normalized()?.ok_or(CliFailure::WorkingDirectoryRequired)?;
    let files = taplo.collect_files(&cwd, &config, command.files.into_iter()).await?;
    let mut failed = false;

    for file in files {
      if let Err(error) = lint_file(taplo, &file, &config).await {
        tracing::error!(%error, path = ?file, "invalid file");
        failed = true;
      }
    }

    if failed {
      Err(CliFailure::FileValidationFailed.into())
    } else {
      Ok(())
    }
  })
}

/// Lint one host file.
fn lint_file<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  file: &'operation Path,
  config: &'operation Config,
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    let source = String::from_utf8(taplo.env.read_file(file).await?)?;
    lint_source(taplo, file, &source, config).await
  })
}

/// Lint one source document.
fn lint_source<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  file_path: &'operation Path,
  source: &'operation str,
  config: &'operation Config,
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    let parse = parser::parse(source)?;
    let display_path = path_text(file_path)?;
    print_parse_errors(taplo, &SimpleFile::new(display_path, source), parse.diagnostics()).await?;
    if !parse.diagnostics().is_empty() {
      return Err(CliFailure::SyntaxErrors.into());
    }

    let dom = parse.into_dom();
    if let Err(errors) = dom.validate() {
      print_semantic_errors(taplo, &SimpleFile::new(display_path, source), errors.into_iter()).await?;
      return Err(CliFailure::SemanticErrors.into());
    }

    if !config.is_schema_enabled(file_path) {
      tracing::debug!("schema validation disabled for file");
      return Ok(());
    }

    let file_url = if file_path == Path::new("-") {
      Url::parse("file:///stdin.toml").map_err(|url_error| CliError::Url {
        input:  "file:///stdin.toml".into(),
        source: url_error,
      })?
    } else {
      Url::from_file_path(file_path).map_err(|()| CliError::FileUrl {
        path: file_path.into()
      })?
    };
    taplo.schemas.associations().add_from_document(&file_url, &dom)?;

    if let Some(schema_association) = taplo.schemas.associations().association_for(&file_url) {
      let schema_name = schema_association.meta.get("name").and_then(JsonValue::as_str).unwrap_or("");
      let schema_source = schema_association.meta.get("source").and_then(JsonValue::as_str).unwrap_or("");
      tracing::debug!(
          schema.url = %schema_association.url,
          schema.name = schema_name,
          schema.source = schema_source,
          "using schema"
      );
      let errors = taplo.schemas.validate_root(&schema_association.url, &dom).await?;
      if !errors.is_empty() {
        print_schema_errors(taplo, &SimpleFile::new(display_path, source), &errors).await?;
        return Err(CliFailure::SchemaValidation.into());
      }
    }
    Ok(())
  })
}
