//! Conversions between the internal [`url::Url`] and the LSP wire type [`lsp_types::Uri`].
//!
//! lsp-types 0.97 replaced its `Url` re-export (the `url` crate) with `Uri` (a `fluent_uri`
//! newtype). taplo, taplo-common, and this server model document and schema locations with
//! `url::Url`, so standard LSP params/results are converted at the protocol boundary while
//! everything internal stays `url::Url`.

use std::str::FromStr;

use lsp_types::Uri;
use url::Url;

/// Convert an internal [`Url`] into the LSP wire [`Uri`].
///
/// A [`Url`] always renders as an absolute, already-encoded URI, which is valid `Uri` syntax, so
/// this conversion does not fail in practice.
#[must_use]
pub(crate) fn to_uri(url: &Url) -> Uri {
    Uri::from_str(url.as_str()).expect("a url::Url is always a valid lsp_types::Uri")
}

/// Convert an LSP wire [`Uri`] into an internal [`Url`], returning [`None`] if it is not a valid
/// absolute URL.
#[must_use]
pub(crate) fn to_url(uri: &Uri) -> Option<Url> {
    Url::parse(uri.as_str()).ok()
}
