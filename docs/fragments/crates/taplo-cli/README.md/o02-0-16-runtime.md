## `0.16` runtime contract

The native binary returns typed `CliError` values through the reusable command layer and maps them to process output and `ExitCode` only at the binary boundary. Its LSP command uses the concurrent server and the actual native environment supplied by the CLI.
