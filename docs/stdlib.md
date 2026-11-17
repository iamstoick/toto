# `gzv.std` — standard library (Phase 5, Stage 2)

Nine modules of ordinary Toto on the frozen 27 builtins (plus the
FFI modules, which call real C). Import by canonical path; the
sources in `stdlib/*.gzv` are embedded in every `toto` binary (see
[project.md](project.md)).

```gzv
import "gzv.std/strings.gzv";
import "gzv.std/json.gzv";

fn main() -> int {
    println(pad_left("42", 5, "0"));
    println(json_stringify(json_int(42)));
    return 0;
}
```

Conventions across all modules:

- No new builtins were added: the frozen catalog stands. Character
  access goes through `split(s, "")` (the builtin splits into Unicode
  scalar values); string building is `+` concatenation.
- No user generics exist in v1, so element-wise helpers come in
  per-type variants (`map_ints`, `map_strings`, …). `int` and
  `string` are covered; the pattern extends on demand.
- Flat namespace: every public *and private* stdlib item shares one
  program-wide scope with user code. Private helpers carry a
  module prefix (`json_skip_ws`, …); defining a top-level item with
  the same name as any stdlib item is `E0311`. Public names are the
  API below — treat them as reserved.
- Checked arithmetic applies: `pow_int`/`factorial` panic on overflow
  rather than wrap.

## `strings`

Predicates, joining, and slicing on the `split(s, "")` character path.
Indices are character (scalar) positions, agreeing with `len`.

| Function | Behavior |
|---|---|
| `starts_with(s, prefix)` | `split`-based prefix test; empty prefix is true |
| `ends_with(s, suffix)` | mirror of the above |
| `join(parts, sep)` | concatenate with separator; empty array joins to `""` |
| `repeat(s, n)` | `s` concatenated `n` times (`n = 0` gives `""`) |
| `replace(s, from, to)` | all non-overlapping occurrences; empty `from` returns `s` |
| `char_at(s, i)` | one-character string; out-of-range panics `E0209` |
| `substring(s, start, end)` | half-open character range; out-of-range panics `E0209` |
| `index_of(s, needle)` | first character index, or `-1`; empty needle is `0` |
| `pad_left(s, width, pad)` | prepend `pad` until `len` reaches `width` |
| `pad_right(s, width, pad)` | append `pad` until `len` reaches `width` |
| `is_empty(s)` | `len(s) == 0` |

## `collections`

Higher-order array helpers. Functions pass as values (named or
inline closures); all builders return fresh arrays, while `sort_ints`
and the `reverse_*` sort in place (arrays are shared by reference)
and return the same array for chaining.

Ints: `map_ints`, `filter_ints`, `fold_ints`, `sum_ints`,
`any_int`, `all_int` (`all` on `[]` is true), `find_index_int`
(`Option<int>`), `contains_int`, `sort_ints` (insertion sort),
`reverse_ints`.
Strings: `map_strings`, `filter_strings`, `contains_string`,
`reverse_strings`, `find_index_string`.

```gzv
import "gzv.std/collections.gzv";

fn is_big(x: int) -> bool {
    return x > 2;
}

fn main() -> int {
    println(sum_ints(filter_ints([1, 2, 3, 4], is_big)));
    return 0;
}
```

## `math`

Exact integer operations beyond the builtins
(`abs`/`min`/`max`/`sqrt`/`floor`/`ceil` stay builtins):
`clamp_int`, `clamp_float`, `pow_int` (non-negative exponent;
`pow_int(b, 0) == 1`), `gcd`, `lcm` (`lcm(0, x) == 0`), `factorial`,
`is_prime` (trial division; `1` is not prime).

## `time`

Pure durations: a duration is an `int` count of milliseconds, and a
timestamp is an `int` count of milliseconds since an
application-chosen epoch. **There is no wall clock**: the frozen
builtins expose no clock, and the catalog stays frozen. Wall-clock
constructors land with the Stage 3 FFI work; until then this module
covers timeout math, backoff, and log rendering.

Constructors `duration_from_millis/secs/mins/hours`, projection
`duration_as_millis/secs` (truncating), `duration_add/sub`,
three-way `duration_compare` (`-1`/`0`/`1`), `format_duration`
(`"500ms"`, `"90s"`, `"2m 5s"`, `"3h 4m"`, `"2d 1h"`, `"-"` prefix
for negatives), and `timestamp_add`/`timestamp_diff` intent markers.

## `io`

`read_lines(path)` splits `read_file` on `"\n"`, dropping one
trailing empty (a trailing newline terminates the last line; it is
not an extra line — mirrors `str::lines`; interior empties are kept).
`write_lines(path, lines)` joins with `"\n"` and overwrites.
`append_line` is read-modify-write and **not atomic**: concurrent
appends can lose lines; serialize appends through a channel instead.
`print_lines` prints each element.

Tests seed work files by overwrite first: there is no delete
builtin, so the suite never relies on a file being absent.

## `fs`

Pure path surgery (`path_join`, `basename`, `extension`, `stem`,
`is_absolute` — `/`-separated, no normalization); file
reads/writes stay the builtins. `stem("archive.tar.gz")` is
`"archive.tar"`.

## `json`

Parse (`json_parse`) and serialize (`json_stringify`) with typed
accessors over a `JsonVal` struct:

```gzv
struct JsonVal {
    kind: string,      // "null" | "bool" | "num" | "str" | "arr" | "obj"
    raw: string,       // bool/number literal, or unescaped string content
    items: Array<string>, // arr elements as compact JSON
    keys: Array<string>,  // obj keys
    vals: Array<string>   // obj values as compact JSON
}
```

Nested values store as **compact JSON strings**, parsed on access:
Toto v1 rejects recursive types (`E0302`), so a directly nested DOM
is inexpressible and this strata design is the honest encoding.
`json_array_get`/`json_object_get` re-parse the nested slice on every
access (`O(depth)` per descent, no caching).

Constructors: `json_null/bool/int/float/string`,
`json_array(elems: Array<JsonVal>)`,
`json_object(keys, vals)` (`Result`: `Err` on length mismatch).
Accessors return `Result` with plain messages:
`json_kind/as_bool/as_int/as_float/as_string`,
`json_array_len/get` (`Err` on wrong kind or out-of-bounds),
`json_object_get` (`Err` on wrong kind or missing key).

Recorded limits (all covered by tests in `stdlib/tests/`):

- Numbers keep their raw literal: `json_as_int` rejects non-integers
  and `parse_int` panics `E0210` past i64 range (inherited, not
  wrapped). Floats parse via float arithmetic — exact decimal
  semantics is not guaranteed — and serialize via `to_string`
  (Rust `Debug`, valid JSON).
- `\uXXXX` escapes are rejected with a clear error: no
  codepoint-to-character builtin exists in the frozen catalog.
- Other C0 controls pass through raw on output.
- Depth is bounded by interpreter/JIT stack limits, not by this
  module; keep nesting shallow.
- Error messages are the contract: `t_errors` pins each one
  (`"unterminated string"`, `"trailing characters after JSON
  value"`, `"unicode escapes (\\u) are not supported"`, …).

## `ffi`

C ABI wrappers over real libc/libm/libpthread (see [ffi.md](ffi.md)
for the per-binding ownership/encoding/thread-safety contracts).
Non-callback wrappers build: `apps/wordfreq` (per-line `c_strlen`)
links a standalone binary with byte-identical output. Foreign
threads live in `gzv.std/ffi_thread`, which is run/test-only —
`c_thread_spawn` hands a callback to `pthread_create`, and a called
callback site refuses at compile time (exit 3). Every wrapper's
contract (strings as NUL-copies, `open` failing as `-errno`,
callbacks drained on the joining worker) is pinned by
`test_ffi.gzv` and the `proj11_ffi` corpus project.

## Execution paths

| Path | Status | Evidence |
|---|---|---|
| Interpreter (`toto run` / `toto test`) | Full suite green | `toto: 8 passed, 0 failed` in `stdlib/`; corpus `proj09`/`proj10` goldens |
| Runtime executor (concurrent) | Green | `test_concurrent.gzv` (pipeline over channels) |
| Built executable (`toto build`) | Output-equivalent | `stdlib` smoke: binary stdout byte-identical to interpreter |
| JIT via `toto dev` | Unsupported | `dev` is single-file scope (`parse_and_check` only); multi-file dev loop is the known carry-forward |

`test_concurrent.gzv` uses the corpus counted-recv idiom (no
for-over-channel, which the corpus never exercises).
