# Worked `just x` Extension Command

The template ships with an intentionally empty repository-specific extension registry. Standard workflows remain owned by the installed `template` binary; `just x` is the only consumer-compiled command seam.

This example adds `just x release-notes -- --since <REF>` without changing the `justfile`, the installed catalog, or any reusable workflow crate.

## Add the parser dependency

The empty registry already depends on `template-core` and `template-stask`. Add `bpaf` only when the repository gains its first typed extension parser:

```toml
[dependencies]
bpaf.workspace = true
template-core.workspace = true
template-stask.workspace = true
```

## Register the command

Replace the empty body of `stask/src/extensions.rs` with a typed repository-owned command enum and registry:

```rust
//! Consumer-owned registry for `just x <name>` commands.

use bpaf::Parser as _;
use bpaf::construct;
use bpaf::long;
use template_core::cli::command::CommandSet;
use template_core::cli::output::plain;

/// Arguments accepted by the release-note extension.
#[derive(Clone, Debug)]
struct ReleaseNotes {
  /// Base Git reference to compare against.
  since: String,
}

/// Every repository-specific extension command.
#[derive(Clone, Debug)]
enum ProjectCommand {
  /// Generate release notes from Git history.
  ReleaseNotes(ReleaseNotes),
}

/// Build the repository extension registry.
///
/// # Errors
///
/// Returns a typed metadata or duplicate-name error before runner construction.
pub fn commands() -> template_stask::Result<CommandSet> {
  let since = long("since")
    .help("Base Git reference to compare against")
    .argument::<String>("REF")
    .fallback("HEAD".to_owned());
  let options = construct!(ReleaseNotes {
    since
  })
  .to_options();
  let release_notes = template_stask::extension_command(
    "release-notes",
    "Generate release notes from Git history",
    options,
    ProjectCommand::ReleaseNotes,
  )?;

  template_stask::registry(
    "repository extensions",
    vec![release_notes],
    |command, context| match command {
      ProjectCommand::ReleaseNotes(ReleaseNotes {
        since,
      }) => context.stdout(&plain(format!(
        "building release notes since {since} from {}",
        context.invocation_dir().display()
      ))),
    },
  )
}
```

`template-stask` rejects duplicate nested names before constructing the runner. Its top-level command set contains only `x` and is classified as `StaskExtension`, so a repository extension cannot masquerade as an installed standard command.

## Run the extension

The existing recipe forwards the directory from which `just` was invoked and places extension-owned dashed options after the passthrough separator:

```bash
just x release-notes -- --since v1.2.0
```

Per-command help and parser failures use the shared `bpaf` renderer:

```bash
just x release-notes -- --help
```

Direct `stask` invocation remains guarded. Use `just x` locally; CI may invoke the extension runner with its existing `CI` signal.
