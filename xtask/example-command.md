# Worked `just x` Extension Command

The template ships with an empty local extension registry. That is intentional: `just x` is a stable consumer-owned seam, but no sample command is compiled into the template. Before a consumer adds commands, `just x` exits nonzero with:

```text
no extension commands are registered (add one in xtask/src/extensions.rs)
```

Use this document as the worked reference for adding the first project-specific command. The example below adds `just x release-notes -- --since <REF>` without changing reusable workflow crates, the `justfile`, CI, or any built-in command.

## Files Changed

Adding a project command touches three existing local files and adds one new module:

```text
xtask/Cargo.toml       # add `bpaf.workspace = true` for the first bpaf-backed command
xtask/src/
├── lib.rs              # append `pub mod release_notes;`
├── extensions.rs       # add the enum variant, run arm, and parser entry
└── release_notes.rs    # new command module
```

`strict-xtask-core` owns the generic router through `extension_command_set`: parsing `x --from`, joining post-`--` passthrough tokens, rendering `bpaf` help/errors, handling the empty registry, and rebasing `CommandContext::invocation_dir()`.

Because the empty template `xtask` does not depend on `bpaf` directly, add the direct dependency before creating the command module:

```toml
[dependencies]
bpaf.workspace = true
strict-xtask-agents-md.workspace = true
strict-xtask-cargo.workspace = true
strict-xtask-core.workspace = true
```

## Command Module

Create `xtask/src/release_notes.rs`:

```rust
//! Release-note extension command: `just x release-notes -- --since <REF>`.

use bpaf::OptionParser;
use bpaf::Parser as _;
use bpaf::construct;
use bpaf::long;
use strict_xtask_core::CommandContext;
use strict_xtask_core::ExtensionParser;
use strict_xtask_core::extension_command;
use strict_xtask_core::output::StatusKind;
use strict_xtask_core::output::plain;

use crate::extensions::ProjectCommand;

/// Parsed, validated arguments for `just x release-notes`.
#[derive(Clone, Debug)]
pub(crate) struct Args {
  /// Base git ref to compare against.
  since: String,
}

/// Declare the command's `bpaf` options.
#[allow(
  clippy::single_call_fn,
  reason = "a named options() keeps the extension parser beside the command it configures"
)]
fn options() -> OptionParser<Args> {
  let since = long("since")
    .help("Base git ref to compare against")
    .argument::<String>("REF")
    .fallback("HEAD".to_owned());

  construct!(Args {
    since
  })
  .to_options()
  .descr("Generate release notes from git history")
}

/// Run the extension command.
///
/// # Errors
///
/// Returns an output error if writing a status line fails.
#[allow(
  clippy::single_call_fn,
  reason = "extension dispatch calls the handler once through ProjectCommand::run"
)]
pub(crate) fn execute(context: &CommandContext, args: Args) -> strict_xtask_core::Result<()> {
  let Args {
    since,
  } = args;
  let invocation_dir = context.invocation_dir();

  context.output().status(
    StatusKind::Info,
    plain(format!(
      "building release notes since {since} from {}",
      invocation_dir.display()
    )),
  )
}

/// Registry entry point referenced from `xtask/src/extensions.rs`.
#[must_use]
#[allow(
  clippy::single_call_fn,
  reason = "the command() seam is consumed once by the local extension registry"
)]
pub fn command() -> ExtensionParser<ProjectCommand> {
  extension_command(
    "release-notes",
    "Generate release notes from git history",
    options(),
    ProjectCommand::ReleaseNotes,
  )
}
```

Keep command arguments in the command module. The registry should only know the enum payload type and dispatch target.

## Register The Module

Append the module in `xtask/src/lib.rs`:

```rust
pub mod extensions;
pub mod release_notes;
```

Then update `xtask/src/extensions.rs` from the empty registry to an inhabited one:

```rust
//! Consumer-owned extension registry for `just x <name>` commands.

use strict_xtask_core::CommandContext;
use strict_xtask_core::CommandSet;
use strict_xtask_core::ExtensionParser;
use strict_xtask_core::extension_command_set;

use crate::release_notes;

/// Every extension command this repository exposes under `just x <name>`.
#[derive(Clone, Debug)]
pub enum ProjectCommand {
  /// Generate release notes from git history.
  ReleaseNotes(release_notes::Args),
}

impl ProjectCommand {
  /// Run the command selected by the extension parser.
  ///
  /// # Errors
  ///
  /// Propagates the selected command's error.
  fn run(self, context: &CommandContext) -> strict_xtask_core::Result<()> {
    match self {
      Self::ReleaseNotes(args) => release_notes::execute(context, args),
    }
  }
}

/// Build the local extension command set.
///
/// # Errors
///
/// Returns an error if the top-level extension router cannot be registered.
#[allow(
  clippy::single_call_fn,
  reason = "local xtask composition consumes this command set once when building the runner"
)]
pub fn commands() -> strict_xtask_core::Result<CommandSet> {
  extension_command_set(
    "local xtask extensions",
    "x",
    "Run a consumer-registered extension command",
    "no extension commands are registered (add one in xtask/src/extensions.rs)",
    parsers(),
    ProjectCommand::run,
  )
}

/// Build every extension parser in registration order.
#[allow(
  clippy::single_call_fn,
  reason = "the parser list is named so new local extension registrations have a single obvious insertion point"
)]
fn parsers() -> Vec<ExtensionParser<ProjectCommand>> {
  vec![release_notes::command()]
}
```

## Run It

Use `just x <name>` for the selected command and put the command's own dashed flags after `--`:

```bash
just x release-notes -- --since v1.2.0
```

The `justfile` recipe forwards `--from "{{invocation_directory()}}"`, so `context.invocation_dir()` is the directory where the user ran `just`, even though the runner anchors reusable workflows at the workspace root.

Per-command help comes from the command module's `bpaf` parser:

```bash
just x release-notes -- --help
```

Unknown names, missing arguments, validation failures, and the empty-registry case all go through the shared `strict-xtask-core` parser/error renderer.
