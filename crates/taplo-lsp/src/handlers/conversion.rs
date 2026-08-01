//! Stateless TOML and JSON conversion request handling.

use serde_json::Value;
use taplo::parser::parse;
use taplo_common::convert;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;

use crate::lsp_ext::request::ConvertToJsonParams;
use crate::lsp_ext::request::ConvertToJsonResponse;
use crate::lsp_ext::request::ConvertToTomlParams;
use crate::lsp_ext::request::ConvertToTomlResponse;

/// Convert TOML input to JSON while preserving already-valid JSON text.
///
/// # Errors
///
/// Returns [`RpcError`] when required request parameters are absent.
#[tracing::instrument(skip_all)]
pub(super) async fn convert_to_json(params: Params<ConvertToJsonParams>) -> Result<ConvertToJsonResponse, RpcError> {
  let parameters = params.required()?;

  if serde_json::from_str::<Value>(&parameters.text).is_ok() {
    return Ok(ConvertToJsonResponse {
      text:  Some(parameters.text),
      error: None,
    });
  }

  match convert::toml_to_json(&parameters.text) {
    Ok(text) => Ok(ConvertToJsonResponse {
      text:  Some(text),
      error: None,
    }),
    Err(err) => Ok(ConvertToJsonResponse {
      text:  None,
      error: Some(err.to_string()),
    }),
  }
}

/// Convert JSON input to TOML while preserving already-valid TOML text.
///
/// # Errors
///
/// Returns [`RpcError`] when required request parameters are absent.
#[tracing::instrument(skip_all)]
pub(super) async fn convert_to_toml(params: Params<ConvertToTomlParams>) -> Result<ConvertToTomlResponse, RpcError> {
  let parameters = params.required()?;

  match parse(&parameters.text) {
    Ok(parsed) if parsed.diagnostics().is_empty() => {
      return Ok(ConvertToTomlResponse {
        text:  Some(parameters.text),
        error: None,
      });
    }
    Ok(_) => {}
    Err(error) => {
      return Ok(ConvertToTomlResponse {
        text:  None,
        error: Some(error.to_string()),
      });
    }
  }

  match convert::json_to_toml(&parameters.text, false) {
    Ok(text) => Ok(ConvertToTomlResponse {
      text:  Some(text),
      error: None,
    }),
    Err(error) => Ok(ConvertToTomlResponse {
      text:  None,
      error: Some(error.to_string()),
    }),
  }
}
