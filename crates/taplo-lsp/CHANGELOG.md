<!-- Do not edit; generated file. -->

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

## 0.8.0

### Features

- Improve error locations for unexpected properties ([#664](https://github.com/tamasfe/taplo/pull/664))

## 0.7.2

Re-release of 0.7.1

## 0.7.1

### Fixes

- Do not enable default-tls unconditionally ([#554](https://github.com/tamasfe/taplo/pull/554))

## 0.7.0

### Fixes

- fix: language server include rule matching ([#378](https://github.com/tamasfe/taplo/pull/378))
- fix hover content ([#453](https://github.com/tamasfe/taplo/pull/453))
