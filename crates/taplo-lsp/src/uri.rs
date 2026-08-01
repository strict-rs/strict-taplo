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
  use std::str::FromStr as _;

  use lsp_types::Position;
  use lsp_types::Uri;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::rowan::TextRange;
  use taplo::rowan::TextSize;
  use taplo_lsp_async::util::Mapper;
  use taplo_lsp_async::util::MappingError;
  use url::Url;

  use super::to_lsp_range;
  use super::to_uri;
  use super::to_url;

  #[test]
  fn uri_conversion_accepts_absolute_urls_and_rejects_relative_wire_uris() -> Result<(), TestFailure> {
    let url = ensure_ok(Url::parse("file:///workspace/file.toml"), "the absolute URL fixture must parse")?;
    let uri = ensure_some(to_uri(&url), "an absolute URL must convert to a wire URI")?;
    ensure(
      to_url(&uri) == Some(url),
      "the URL and URI boundary must round-trip absolute document locations",
    )?;

    let relative = ensure_ok(
      Uri::from_str("workspace/file.toml"),
      "the relative URI reference fixture must parse",
    )?;
    ensure(
      to_url(&relative).is_none(),
      "a relative wire URI must not become an internal absolute URL",
    )
  }

  #[test]
  fn range_conversion_is_exact_or_typed() -> Result<(), TestFailure> {
    let mapper = ensure_ok(Mapper::new_utf16("alpha\n\u{3b2}eta"), "the mapper fixture must build")?;
    let source_range = TextRange::new(TextSize::from(0), TextSize::from(5));
    let lsp_range = ensure_ok(to_lsp_range(&mapper, source_range), "a source-backed range must map")?;
    ensure(
      lsp_range == lsp_types::Range::new(Position::new(0, 0), Position::new(0, 5)),
      "mapped source coordinates must remain exact",
    )?;
    ensure(
      matches!(
        to_lsp_range(&mapper, TextRange::new(TextSize::new(100), TextSize::new(101))),
        Err(MappingError::OffsetOutOfBounds { .. })
      ),
      "source offsets absent from the mapper must return a typed failure",
    )
  }
}
