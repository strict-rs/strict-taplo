use codespan_reporting::files::SimpleFile;
use taplo::dom::Keys;
use taplo::dom::Node;
use taplo::parser;
use taplo_common::environment::LocalEnvironment;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;

use crate::CliError;
use crate::CliFailure;
use crate::LocalCommandFuture;
use crate::Taplo;
use crate::args::GetCommand;
use crate::args::OutputFormat;
use crate::path_text;
use crate::printing::print_parse_errors;
use crate::printing::print_semantic_errors;

/// Execute one TOML query and write its selected representation.
pub(super) fn execute_get<E: LocalEnvironment>(taplo: &Taplo<E>, command: GetCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    let mut stdout = taplo.env.stdout();

    // `--separator` should only be handled for a text output format
    if command.separator.is_some() && !matches!(command.output_format, OutputFormat::Value) {
      return Err(CliFailure::InvalidSeparator.into());
    }

    let source = if let Some(path) = command.file_path.as_ref() {
      String::from_utf8(taplo.env.read_file(path).await?)?
    } else {
      let mut stdin = taplo.env.stdin();
      let mut source_text = String::new();
      let bytes_read = stdin.read_to_string(&mut source_text).await?;
      tracing::trace!(bytes_read, "read query input from standard input");
      source_text
    };

    let parse = parser::parse(&source)?;

    let file_path = match command.file_path.as_deref() {
      Some(path) => path_text(path)?,
      None => "-",
    };

    print_parse_errors(taplo, &SimpleFile::new(file_path, &source), parse.diagnostics()).await?;

    if !parse.diagnostics().is_empty() {
      return Err(CliFailure::SyntaxErrors.into());
    }

    let node = parse.into_dom();

    if let Err(errors) = node.validate() {
      print_semantic_errors(taplo, &SimpleFile::new(file_path, &source), errors.into_iter()).await?;

      return Err(CliFailure::SemanticErrors.into());
    }

    let output = match command.output_format {
      OutputFormat::Json => json_output(&node, command.pattern.as_deref(), command.strip_newline)?,
      OutputFormat::Value => value_output(
        &node,
        command.pattern.as_deref(),
        command.separator.as_deref().unwrap_or("\n"),
        command.strip_newline,
      )?,
      OutputFormat::Toml => toml_output(&node, command.pattern.as_deref(), command.strip_newline)?,
    };
    stdout.write_all(&output).await?;
    stdout.flush().await?;
    Ok(())
  })
}

/// Resolve one optional dotted query into its matched semantic nodes.
fn query_nodes(node: &Node, query_pattern: &str) -> Result<Vec<Node>, CliError> {
  let keys = query_pattern.trim_start_matches('.').parse::<Keys>()?;
  let nodes = node
    .find_all_matches(&keys, false)?
    .map(|(_path, matched)| matched)
    .collect::<Vec<_>>();
  if nodes.is_empty() {
    return Err(CliFailure::NoQueryMatches.into());
  }
  Ok(nodes)
}

/// Serialize JSON query output with its command-level newline policy.
#[allow(
  clippy::single_call_fn,
  reason = "JSON output owns the single-match versus match-list wire shape independently of value and TOML rendering"
)]
fn json_output(node: &Node, pattern: Option<&str>, strip_newline: bool) -> Result<Vec<u8>, CliError> {
  let mut output = if let Some(query_pattern) = pattern {
    let nodes = query_nodes(node, query_pattern)?;
    if nodes.len() == 1 {
      let matched = nodes.first().ok_or(CliFailure::NoQueryMatches)?;
      serde_json::to_vec_pretty(matched)?
    } else {
      serde_json::to_vec_pretty(&nodes)?
    }
  } else {
    serde_json::to_vec_pretty(node)?
  };
  if !strip_newline {
    output.push(b'\n');
  }
  Ok(output)
}

/// Render scalar and array query output with the selected separator.
#[allow(
  clippy::single_call_fn,
  reason = "value output centralizes recursive scalar extraction and separator joining before transport writes"
)]
fn value_output(node: &Node, pattern: Option<&str>, separator: &str, strip_newline: bool) -> Result<Vec<u8>, CliError> {
  let mut output = if let Some(query_pattern) = pattern {
    query_nodes(node, query_pattern)?
      .iter()
      .map(|matched| extract_value(matched, separator))
      .collect::<Result<Vec<_>, _>>()?
      .join(separator)
  } else {
    extract_value(node, separator)?
  };
  if !strip_newline {
    output.push('\n');
  }
  Ok(output.into_bytes())
}

/// Render TOML query output while preserving the CLI's list envelope contract.
#[allow(
  clippy::single_call_fn,
  reason = "TOML output owns the distinct single-node and multi-node document envelopes before newline normalization"
)]
fn toml_output(node: &Node, pattern: Option<&str>, strip_newline: bool) -> Result<Vec<u8>, CliError> {
  let mut output = if let Some(query_pattern) = pattern {
    let nodes = query_nodes(node, query_pattern)?;
    if nodes.len() == 1 {
      let matched = nodes.first().ok_or(CliFailure::NoQueryMatches)?;
      matched.to_toml(false, false)?
    } else {
      let mut rendered = String::from("[\n");
      for matched in nodes {
        rendered += "  ";
        rendered += &matched.to_toml(true, false)?;
        rendered += ",\n";
      }
      rendered += "]\n";
      rendered
    }
  } else {
    node.to_toml(false, false)?
  };
  apply_trailing_line_policy(&mut output, strip_newline);
  Ok(output.into_bytes())
}

/// Apply the query command's trailing-line policy to rendered text.
fn apply_trailing_line_policy(output: &mut String, strip_newline: bool) {
  if strip_newline {
    if output.ends_with('\n') {
      *output = output.trim_end().to_owned();
    }
    return;
  }
  if !output.ends_with('\n') {
    output.push('\n');
  }
}

/// Render one semantic node through the scalar-value output contract.
fn extract_value(node: &Node, separator: &str) -> Result<String, CliError> {
  Ok(match *node {
    Node::Table(_) => {
      return Err(CliFailure::TableValueOutput.into());
    }
    Node::Array(ref array) => {
      let mut values = Vec::new();

      for element in array.items() {
        values.push(extract_value(&element, separator)?);
      }

      values.join(separator)
    }
    Node::Bool(ref boolean) => boolean.value().to_string(),
    Node::Str(ref string) => string.value().to_owned(),
    Node::Integer(ref integer) => integer.value().to_string(),
    Node::Float(ref float) => float.value().to_string(),
    Node::Date(ref date_value) => date_value.value().to_string(),
    Node::Invalid(_) => return Err(CliFailure::SemanticErrors.into()),
  })
}
