# Toto projects: modules, build, test (Phase 5, Stage 1)

Single-file programs keep working unchanged (`toto run file.gzv`
resolves imports against the file's directory). A multi-file
*project* is a directory with a `toto.toml` manifest.

## Module system

- A top-level `import "path";` pulls another file's items into the
  single flat top-level namespace. Import paths resolve lexically
  against the importing file's directory (`lib/a.gzv` from
  `lib/b.gzv` means `lib/shared.gzv` lives at `lib/shared.gzv` only
  if imported as `"shared.gzv"`).
- `pub` may prefix any top-level item and has no effect: no private
  items, no nested modules. `mod` / `use` are not in v1.
- Rules (spec §3.1): an import the driver cannot resolve is `E0401`
  (single-file mode, or file not found); a file importing itself,
  directly or transitively, is `E0402` (reported at the import that
  closes the cycle); duplicate top-level names across files are
  `E0311`, reported at the later file in load order (entry first).
- Diagnostics carry their origin file: static errors render as
  `path:line:col` with that file's snippet, and runtime panics
  attribute every trace frame to its own file (function names are
  unique program-wide, so attribution is exact).
- Same file imported twice (even spelled differently) loads once;
  diamond imports are fine.

## `toto.toml`

```toml
[project]
name = "myapp"
version = "0.1.0"
entry = "main.gzv"
```

That shape is the whole v1 manifest format: one `[project]` table
with exactly `name`, `version`, `entry` (plain double-quoted
strings, `#` comments and blank lines allowed). The reader is a
strict subset parser and rejects anything else with a line-numbered
error (exit 3); a future full TOML reader still accepts every
manifest written today. `toto build` / `toto test` find the project
root by walking up from the working directory.

## Commands

- `toto init <name>`: scaffold `<name>/toto.toml`,
  `<name>/main.gzv` (hello world), `<name>/tests/example.gzv`.
  Refuses when the directory exists.
- `toto run <file.gzv>`: unchanged, now multi-file capable.
- `toto build [--release]`: compile the project to a standalone
  executable at `<root>/target/<name>`. Object emission reuses the
  JIT lowering core over `ObjectModule` (task-mode semantics, so
  concurrent programs build); static bytes become module data;
  the object links against the `toto-jit` runtime helpers via a
  generated shim crate (nested `cargo build`, needs the source
  tree present). Default is debug + no optimization; `--release`
  enables the `speed` pipeline and release linking.
- `toto test`: run every `tests/*.gzv` through the same load +
  check + execute path as `toto run`; exit 0 is pass. Prints
  `ok <name>` / `FAIL <name> (exit N)` plus a `passed, failed`
  summary; exits 1 on any failure. Programs print only on failure.

## `toto build` internals (for the curious)

- User functions export as `toto_fn_<name>`; bodies as
  `toto_body<id>$`; runtime imports link as `task_*` (stable via
  `#[unsafe(no_mangle)]`).
- The binary embeds its sources and re-resolves them at startup for
  structs, display names, and panic rendering; only machine code is
  ahead-of-time. The `chan_types` table is baked in as generated
  Rust expressions.
- Output equivalence is verified per release: the built binary must
  match `toto run` byte-for-byte (stdout, stderr, exit code) on the
  Stage 1 sample (multi-file panic case, `Result`/`Option`
  sequential case, buffered-channel concurrent case).

## Limits (carry-forwards)

- `toto dev` watches one file: imports resolve, but only the entry
  is watched for changes (multi-file dev loop is a Stage 2 item).
- Runtime panic attribution assumes function names resolve in the
  embedded sources (true for built binaries; same-table design as
  `toto run`).
