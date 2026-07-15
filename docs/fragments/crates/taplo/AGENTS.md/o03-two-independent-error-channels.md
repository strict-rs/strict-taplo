## Two independent error channels

Syntax errors (`Parse::errors`) and DOM/semantic errors (`Node::validate()` / `Node::errors()`) are **separate** — a document can parse cleanly yet fail validation, and a DOM is still built when syntax errors are present. Any code that "checks for errors" must consult both.
