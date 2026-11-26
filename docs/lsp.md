# Toto LSP — hover, go-to-definition, diagnostics (Phase 5, Stage 4)

`toto lsp` speaks LSP over stdio (Content-Length framing) with a
dependency-free hand-rolled JSON layer — no serde, no lsp-server
crate. One open document plus project-aware imports.

## Capabilities

- `hoverProvider`, `definitionProvider` (both full-document and
  cross-file locations carry `file://` URIs).
- `textDocumentSync`: open/close + full-document `didChange`.
- `publishDiagnostics` after every open/change: spanned errors with
  stable codes (`E0101`, `E0301`, …), zero diagnostics on clean files.

Hover shows the checked type (`x: int`), the declaration form
(`let x`), or a signature (`fn double(n: int) -> int`, builtins
included) — documented items append their `///` prose below
the signature fence. Definition jumps to the local declaration or into the
imported file; builtins resolve to null. A broken file keeps its
last good data out: hover answers null and diagnostics carry the
error codes. A malformed frame is skipped, never fatal — only stdin
EOF ends the session (regression test `lsp_malformed_frame_is_skipped`).

## Verification status (disclosed)

**Headless/protocol-only.** Covered by `crates/toto-cli/tests/lsp.rs`
(3 tests driving `toto lsp` over piped stdio and asserting hover
contents, definition targets, and diagnostic codes). The server has
NOT been exercised inside a real editor — no editor, version, or
session is claimed. Columns are UTF-16 code units per the LSP
standard (Stage 2; previously Unicode scalars — scalar counting
was the documented past behavior, wrong for non-ASCII text in
UTF-16 editors). One document at a time is fully modeled; the open buffer
overrides disk and `gzv.std` is embedded for import-aware answers.

## Limits (accepted)

- Comments vanish nowhere (the server reads source, it does not
  format); hover surfaces `///` item prose, and definition
  jumps land on documented declarations unchanged.
- No completion, references, rename, or code actions (Enterprise
  Wave 5 reserves richer LSP work).
- `file://` URIs only; `untitled:` buffers are refused.
