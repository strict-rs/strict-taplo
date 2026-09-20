//! Conversions between the internal [`url::Url`] and the LSP wire type [`lsp_types::Uri`].
//!
//! lsp-types 0.97 replaced its `Url` re-export (the `url` crate) with `Uri` (a `fluent_uri`
//! newtype). taplo, taplo-common, and this server model document and schema locations with
//! `url::Url`, so standard LSP params/results are converted at the protocol boundary while
//! everything internal stays `url::Url`.

use std::str::FromStr as _;

use lsp_types::Range;
use lsp_types::Uri;
use taplo::rowan::TextRange;
use taplo_lsp_async::rpc::RpcError;
use taplo_lsp_async::util::Mapper;
use taplo_lsp_async::util::MappingError;
use url::Url;

/// Convert an internal [`Url`] into the LSP wire [`Uri`].
#[must_use]
pub(super) fn to_uri(url: &Url) -> Option<Uri> {
  Uri::from_str(url.as_str()).ok()
}

/// Convert an LSP wire [`Uri`] into an internal [`Url`], returning [`None`] if it is not a valid
/// absolute URL.
#[must_use]
pub(super) fn to_url(uri: &Uri) -> Option<Url> {
  Url::parse(uri.as_str()).ok()
}

/// Map a syntax range directly into checked LSP coordinates.
pub(super) fn to_lsp_range(mapper: &Mapper, range: TextRange) -> Result<Range, MappingError> {
  mapper.range(range)
}

/// Translate a checked coordinate failure at the JSON-RPC protocol boundary.
pub(super) fn mapping_rpc_error(error: &MappingError) -> RpcError {
  RpcError::internal_error().with_details(error.to_string())
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::str::FromStr as _;

  use lsp_types::Position;
  use lsp_types::Uri;
  use strict_test_support::ensure_that;
  use taplo::rowan::TextRange;
  use taplo::rowan::TextSize;
  use taplo_lsp_async::util::Mapper;
  use taplo_lsp_async::util::MappingError;
  use url::Url;

  use super::to_lsp_range;
  use super::to_uri;
  use super::to_url;

  #[test]
  fn uri_conversion_accepts_absolute_urls_and_rejects_relative_wire_uris() -> Result<(), impl Debug> {
    let absolute = Url::parse("file:///workspace/file.toml").map(|url| {
      let wire = to_uri(&url);
      let restored = wire.as_ref().and_then(to_url);
      (url, wire, restored)
    });
    let relative = Uri::from_str("workspace/file.toml").map(|wire| {
      let converted = to_url(&wire);
      (wire, converted)
    });
    ensure_that(
      (absolute, relative),
      "absolute document locations must round-trip while relative wire URIs remain unresolved",
      |observed| {
        observed
          .0
          .as_ref()
          .is_ok_and(|roundtrip| roundtrip.1.is_some() && roundtrip.2.as_ref() == Some(&roundtrip.0))
          && observed.1.as_ref().is_ok_and(|rejected| rejected.1.is_none())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn range_conversion_is_exact_or_typed() -> Result<(), impl Debug> {
    let observed = Mapper::new_utf16("alpha\n\u{3b2}eta").map(|mapper| {
      let source_range = TextRange::new(TextSize::from(0), TextSize::from(5));
      let mapped = to_lsp_range(&mapper, source_range);
      let rejected = to_lsp_range(&mapper, TextRange::new(TextSize::new(100), TextSize::new(101)));
      (mapper, mapped, rejected)
    });
    ensure_that(
      observed,
      "source ranges must map exactly or preserve the typed out-of-bounds failure",
      |result| {
        let Ok(ref ranges) = *result else {
          return false;
        };
        ranges.1 == Ok(lsp_types::Range::new(Position::new(0, 0), Position::new(0, 5)))
          && matches!(ranges.2, Err(MappingError::OffsetOutOfBounds { .. }))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
