use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_async_stub::rpc::Error;
use serde_json::Value;
use taplo::dom::Node;
use taplo::parser::parse;
use taplo_common::environment::Environment;

use crate::lsp_ext::request::ConvertToJsonParams;
use crate::lsp_ext::request::ConvertToJsonResponse;
use crate::lsp_ext::request::ConvertToTomlParams;
use crate::lsp_ext::request::ConvertToTomlResponse;
use crate::world::World;

#[tracing::instrument(skip_all)]
pub(crate) async fn convert_to_json<E: Environment>(
  _context: Context<World<E>>,
  params: Params<ConvertToJsonParams>,
) -> Result<ConvertToJsonResponse, Error> {
  let p = params.required()?;

  if serde_json::from_str::<Value>(&p.text).is_ok() {
    return Ok(ConvertToJsonResponse {
      text:  Some(p.text),
      error: None,
    });
  }

  match serde_json::to_string_pretty(&parse(&p.text).into_dom()) {
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

#[tracing::instrument(skip_all)]
pub(crate) async fn convert_to_toml<E: Environment>(
  _context: Context<World<E>>,
  params: Params<ConvertToTomlParams>,
) -> Result<ConvertToTomlResponse, Error> {
  let p = params.required()?;

  let parse = parse(&p.text);
  if parse.errors.is_empty() {
    return Ok(ConvertToTomlResponse {
      text:  Some(p.text),
      error: None,
    });
  }

  let dom = match serde_json::from_str::<Node>(&p.text) {
    Ok(dom) => dom,
    Err(err) => {
      return Ok(ConvertToTomlResponse {
        text:  None,
        error: Some(err.to_string()),
      });
    }
  };

  Ok(ConvertToTomlResponse {
    text:  Some(dom.to_toml(false, false)),
    error: None,
  })
}
