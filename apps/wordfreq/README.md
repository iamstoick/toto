# wordfreq — concurrent word/line statistics (Phase 5 sample app)

Reads `needle.txt` (one search word) and `input.txt` (the corpus),
scans the lines on two green threads, and writes the JSON report to
`report.json` (also printed). The report is a JSON array with the
fixed positional schema `[lines, words, hits, bytes, needle,
workers]` (see `lib/scan.gzv`).

## Why this shape

One worker per chunk, each chunk sent whole over its own channel —
no shared mutable state, no close/sentinel protocol, so the result
is bit-identical under any `--workers` count and in the built
binary. Every line crosses the C boundary once (`c_strlen` from
`gzv.std/ffi` proves FFI per line, not just once at startup);
string splitting/matching comes from `gzv.std/strings`, the report
from `gzv.std/json`, file IO from `gzv.std/io`.

Expected output for the committed fixtures:

```
[6,23,3,138,"toto",2]
```

6 lines, 23 words, 3 lines containing `toto`, 138 bytes
(18+31+23+25+21+20 per line — count them, do not bless).

## Toolchain

```
toto run main.gzv        # interpreter
toto run --workers 4 main.gzv   # M:N, same output
toto test                # 1 file, unit + integration checks
toto build               # standalone target/wordfreq, byte-identical stdout
```

`toto dev` does not apply: it is single-file scope and this app is
three modules (`main` + `lib/scan` + stdlib imports).

## Limits

- No CLI arguments in Toto v1 (frozen builtins): input paths are
  fixed (`input.txt`, `needle.txt` next to `main.gzv`).
- The report schema is positional (array, not object):
  `json_object` is fallible on duplicate keys and v1 has no
  `match`/`unwrap`, so the infallible `json_array` carries the
  documented order instead.
- Only `gzv.std/ffi` (no `ffi_thread`): thread-spawning FFI is
  run/test-only, and the app must stay buildable.
