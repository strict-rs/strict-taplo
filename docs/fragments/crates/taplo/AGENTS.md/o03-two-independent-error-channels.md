## Two independent error channels

Tree-construction failures (`ParseFailure`), recoverable syntax diagnostics (`Parse::diagnostics()`), and DOM diagnostics (`Node::validate()` / `Node::errors()`) are **separate**. A document can build a tree yet contain syntax diagnostics, can parse cleanly yet fail semantic validation, and can still produce a frozen DOM for editor recovery. Any code that requires valid TOML must consult all applicable channels.
