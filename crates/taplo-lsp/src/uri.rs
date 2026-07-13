//! Conversions between the internal [`url::Url`] and the LSP wire type [`lsp_types::Uri`].
//!
//! lsp-types 0.97 replaced its `Url` re-export (the `url` crate) with `Uri` (a `fluent_uri`
//! newtype). taplo, taplo-common, and this server model document and schema locations with
//! `url::Url`, so standard LSP params/results are converted at the protocol boundary while
//! everything internal stays `url::Url`.

use std::str::FromStr;

use lsp_async_stub::util::Mapper;
use lsp_async_stub::util::Position as MapperPosition;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::Uri;
use taplo::rowan::TextRange;
use url::Url;

/// Convert an internal [`Url`] into the LSP wire [`Uri`].
#[must_use]
pub(crate) fn to_uri(url: &Url) -> Option<Uri> {
  Uri::from_str(url.as_str()).ok()
}

/// Convert an LSP wire [`Uri`] into an internal [`Url`], returning [`None`] if it is not a valid
/// absolute URL.
#[must_use]
pub(crate) fn to_url(uri: &Uri) -> Option<Url> {
  Url::parse(uri.as_str()).ok()
}

/// Convert an LSP input position to the mapper's nontruncating coordinate type.
#[must_use]
pub(crate) fn from_lsp_position(position: Position) -> MapperPosition {
  MapperPosition::new(u64::from(position.line), u64::from(position.character))
}

/// Convert a mapper position to LSP coordinates when both values fit on the wire.
#[must_use]
pub(crate) fn to_lsp_position(position: MapperPosition) -> Option<Position> {
  Some(Position::new(
    u32::try_from(position.line).ok()?,
    u32::try_from(position.character).ok()?,
  ))
}

/// Map a syntax range and convert both endpoints to LSP coordinates without truncation.
#[must_use]
pub(crate) fn to_lsp_range(mapper: &Mapper, range: TextRange) -> Option<Range> {
  let range = mapper.range(range)?;
  Some(Range::new(to_lsp_position(range.start)?, to_lsp_position(range.end)?))
}

/// Convert a mapper range to LSP coordinates without truncation.
#[must_use]
pub(crate) fn mapper_range_to_lsp(range: lsp_async_stub::util::Range) -> Option<Range> {
  Some(Range::new(to_lsp_position(range.start)?, to_lsp_position(range.end)?))
}

#[cfg(test)]
mod tests {
  use std::str::FromStr;

  use lsp_async_stub::util::Mapper;
  use lsp_async_stub::util::Position as MapperPosition;
  use lsp_async_stub::util::Range as MapperRange;
  use lsp_types::Position;
  use lsp_types::Uri;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::rowan::TextRange;
  use taplo::rowan::TextSize;
  use url::Url;

  use super::mapper_range_to_lsp;
  use super::to_lsp_position;
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
  fn position_and_range_conversion_are_exact_or_absent() -> Result<(), TestFailure> {
    ensure(
      to_lsp_position(MapperPosition::new(2, 3)) == Some(Position::new(2, 3)),
      "representable mapper coordinates must convert exactly",
    )?;
    let too_large = u64::from(u32::MAX).saturating_add(1);
    ensure(
      to_lsp_position(MapperPosition::new(too_large, 0)).is_none(),
      "a line beyond the LSP width must be rejected",
    )?;
    ensure(
      mapper_range_to_lsp(MapperRange {
        start: MapperPosition::new(0, 0),
        end:   MapperPosition::new(0, too_large),
      })
      .is_none(),
      "an unrepresentable range endpoint must suppress the whole range",
    )?;

    let mapper = Mapper::new_utf16("alpha\nβeta", false);
    let source_range = TextRange::new(TextSize::from(0), TextSize::from(5));
    let mapped = ensure_some(to_lsp_range(&mapper, source_range), "a source-backed range must map")?;
    ensure(
      mapped == lsp_types::Range::new(Position::new(0, 0), Position::new(0, 5)),
      "mapped source coordinates must remain exact",
    )?;
    ensure(
      to_lsp_range(&mapper, TextRange::new(TextSize::new(100), TextSize::new(101))).is_none(),
      "source offsets absent from the mapper must not fabricate a range",
    )
  }
}
