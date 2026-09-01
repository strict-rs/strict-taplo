## Add the parser dependency

The empty registry already depends on `template-core` and `template-stask`. Add `bpaf` only when the repository gains its first typed extension parser:

```toml
[dependencies]
bpaf.workspace = true
template-core.workspace = true
template-stask.workspace = true
```
