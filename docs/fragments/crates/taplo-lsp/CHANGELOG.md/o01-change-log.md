# Change Log

## 0.16.0

### Breaking Changes

- Replace the ambiguous server surface with `LocalServer`/`LocalContext` and `ConcurrentServer`/`ConcurrentContext` families.
- Add explicit local and concurrent world constructors with `Rc` and `Arc` ownership respectively.
- Replace legacy mapper extension types with checked `lsp_types::Position` and `lsp_types::Range` conversions over UTF-8, UTF-16, and UTF-32 checkpoints.
- Replace deprecated document-symbol construction with a Taplo-owned modern wire DTO.

### Reliability

- Serialize state-changing notifications behind a mutation barrier while allowing independent native requests to execute concurrently.
- Publish diagnostics, configuration, workspace, and schema results only when their captured generation remains current.
- Preserve duplicate-ID rejection, cancellation, initialization/shutdown sequencing, deferred output, and writer failures in both runtime families.
