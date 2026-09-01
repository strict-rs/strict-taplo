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
