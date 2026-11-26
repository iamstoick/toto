# LSP hover docs — follow-up design (2026-11-26; awaiting implementation go-ahead)

Goal: `textDocument/hover` shows `///` item prose under the
signature, for declarations and uses, local and cross-file.
Wave 5 Stage 4 built the pipeline (lexer → `Item.doc` →
`fmt`/`toto doc`) but left hover prose out by explicit
non-goal; this closes that named follow-up. Wave 5 itself
stays closed — this is a standalone follow-up, not a Stage 5.

## Design (user-approved scope TBD; no code yet)

- Render shape (forced by `hover_at`, `lsp.rs:1600`): hover is
  already a Markdown ` ```toto ` fence around the signature
  string. Prose appends as `\n\n` + the verbatim doc lines
  after the fence; `kind: markdown` unchanged.
- Uniform rule: every item-kind hover appends its `Item.doc`
  when non-empty — `Fn`, `Struct`, `Extern` (block-level doc,
  same per-decl repetition as `toto doc`), `State`,
  `Migrate`, local (`resolve_at`) and cross-file
  (`resolve_global` over `file_srcs`, `lsp.rs:1318`).
  Declaration and use sites share the path, so both gain
  prose with no extra work.
- Unchanged, byte-identical output: locals, params, struct
  fields (no field docs exist in the AST), builtins (static
  `builtin_doc` text already), undocumented items, imports
  (no hover today — module docs are NOT surfaced; non-goal),
  broken files (null hover stands).
- Helper (one place): `with_doc(body: String, doc: &[String])
  -> String` at each item-hover construction site; no
  signature-text changes, so `fn_sig_text`/`struct_sig_text`
  and their callers are untouched.
- `docs/lsp.md` update (in scope): the hover paragraph gains
  the prose sentence and the "no notion of comments" limit is
  struck (it becomes false). Pre-existing staleness outside
  those two sentences (test counts, editor proof, capability
  list) stays Wave 5's record to amend — flagged here, not
  fixed here.
- No parser/AST/checker/fmt/doc changes; no new builtins; no
  D1–D8/ADR/spec change.

## Acceptance

- Protocol tests (`crates/toto-cli/tests/lsp.rs`): documented
  local fn hover contains its prose; cross-file use hover
  contains the lib prose; undocumented hover byte-identical
  to today (existing `contains` assertions unaffected).
- Real-editor proof by the user (Stage 2 pattern): hover a
  documented stdlib item in VS Code and paste the result.
- `./scripts/check.sh` gate green + CI green on the tip.

## What this does NOT do

- Does NOT write code (implementation starts on explicit
  go-ahead). Does NOT surface module (`//!`) docs, field
  docs, or completion-item documentation. Does NOT refresh
  `docs/lsp.md` beyond the two hover/comments sentences.
  Does NOT reopen Wave 5.

## Implementation record (2026-11-26; awaiting CI)

Built on user go-ahead, exactly per the design above:

- `crates/toto-cli/src/lsp.rs`: `with_doc(body, doc)`
  helper + all 14 item-hover sites wrapped (`resolve_at`
  5, same-file `resolve_global` 5, cross-file 4). No
  signature-text, AST, parser, or checker changes.
- `crates/toto-cli/tests/lsp.rs`: new
  `lsp_hover_shows_item_docs` — cross-file fn + struct
  prose, local decl prose, undocumented byte-identical
  (fenced shape pinned, no-leakage asserts).
- `docs/lsp.md`: the two hover/comments sentences only.
- Verification: 21/21 LSP protocol tests green (20 existing
  unaffected — prior fixtures undocumented so output is
  byte-identical). 1.87 advisory: 4 join-when-fits hunks
  applied (no-closure calls, both oracles join); 1 split
  demand on a 108-char test line IGNORED with evidence
  (23 committed lines in the same file exceed 100 chars and
  CI accepts them — splitting is a version quirk).
- NOT done: real-editor proof (user's part, Stage 2
  pattern — hover a documented stdlib item in VS Code);
  CI green on the tip (this commit triggers it; outcome
  belongs in a future note).
