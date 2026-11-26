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

## CI failure round (2026-11-26; fixed same turn)

Run 45766983164 on head 34960e5: check FAILED at
`cargo fmt --check` on exactly ONE hunk — the line-9
`&format!` in the new test (tests/lsp.rs:959), split
byte-identical per the log. Correction recorded here: the
"23 committed precedents" argument was wrong — whatever
lets those 100+-char lines stand does not extend to this
one, and mechanism-hunting stopped there. Refined operating
rule (6/6 agreement between 1.87 advisory and CI): trust
1.87's demands on lines this task touched, keep ignoring
its demands on untouched baseline drift. Re-verified after
the fix: 1.87 fully clean on both crates' trees plus every
touched test file, 21/21 protocol tests green.
Fixed in this commit; re-validation run pending.

## Closed (CI green 2026-11-26, run 45769609177)

Run 45769609177 on head efaa486 concluded SUCCESS: check +
release-gates + bench + macOS/Linux-ARM probes green (Windows
probe the known informational failure). The hover-docs code is
CI-green. Remaining, both non-blocking: this record commit's
own validation run (lag-by-one formality), and the real-editor
proof — the user's part (hover a documented stdlib item in
VS Code and paste the result).

## Toolchain correction (2026-11-26, recorded here)

The no-local-oracle premise above is superseded: `rustup
component add rustfmt clippy` succeeded — rustfmt
1.11.0-stable + clippy 0.1.100 now run locally. The advisory
rule ("trust 1.87 on touched lines") is retired; verify new
work with the real `cargo fmt --check` and `cargo clippy`
before pushing. CI (`@stable`, possibly newer) stays final.

## Real-editor proof (2026-11-26, user-verified)

Confirmed by user screenshot in the Extension Development
Host: hovering `CtxPair` in `stdlib/context.gzv` shows the
signature plus the migrated prose ("One map entry. Keys need
not be unique in the array ... matching the runtime slot."),
rendered inside the fenced block per the implemented shape.
The follow-up's proof standard (protocol tests + real editor,
Stage-2 pattern) is met: 21/21 protocol tests green, CI green
on run 45769609177, editor proof green here. Follow-up
COMPLETE. Remaining: only the lag-by-one validation run on a
record commit, a future note.
