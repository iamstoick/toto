# Toto Language Specification (v1 core — Phase 1 draft)

Status: **APPROVED with amendments (review 2026-11-14)**. This spec
defines the v1 core subset that the Phase 1 interpreter executes, plus
reserved syntax specified now and implemented in later phases. Nothing
outside this document may be assumed. Decisions the PDF did not fix were
marked **[Proposal]**; each is listed in Appendix A with the review verdict.
The Stage 1 STOP gate is discharged: implementation may proceed.

Sources: Toto Implementation Roadmap v2.3 (authoritative; Part 1 governs
sequencing, Part 2 governs technical defaults D1–D8). Where this spec and the
PDF conflict on SCOPE for Phase 1, the Phase 1 brief wins; on all technical
decisions, the PDF wins. Terminology: "rooting", not "pinning".

Ratified inputs (2026-11-14, not re-litigated here): checked integer
arithmetic with panic on overflow; 64 KiB FIXED green-thread stacks (not
exercised by the interpreter); `toto dev` drain timeout 5 s (Phase 4);
typed `Result<T, E>` with built-in `?` conversion preserving the original
error as cause; MIT OR Apache-2.0; repo `https://github.com/iamstoick/toto`;
GitHub Actions, Linux x86_64.

## 1. Scope: what runs in Phase 1

Phase 1 executes a **core subset**. Everything else in this spec is either
*reserved syntax* (lexed, parsed only far enough to reject cleanly) or a
*Phase 2+ specification* (defined here so Phase 1 diagnostics and corpus
design do not contradict it).

Core subset (implemented by the Phase 1 interpreter):

- `let` / `mut` bindings with optional type annotations (parsed, stored,
  **not checked** in Phase 1; §5.6).
- Values: 64-bit integers, 64-bit floats, strings, booleans, the unit
  value `()`, arrays, structs, ranges, first-class functions and closures.
- Operators: arithmetic `+ - * / %`, unary `-` and `!`, comparison
  `== != < <= > >=`, short-circuit logical `&& ||`, call, index, field
  access, assignment `=`.
- Control flow: `if` / `else` (expressions), `while` and `for` loops
  (**statements**, not expressions), `break` / `continue`, `return`.
- Functions with parameters and explicit return types; recursion; closures.
- Struct declarations, struct literals, field access and field assignment.
- Array literals, indexing, element assignment; built-in iteration.
- Annotations (`@name`, `@name(args)`): parsed and stored, never checked.
- Builtins only (§7): print, string operations, basic file IO, math,
  conversions, array helpers. No other stdlib exists yet.

Explicitly **not executed** in Phase 1 (rejected with phase-naming
diagnostics, §3.5):

| Construct | Decided in | Diagnostic names |
|---|---|---|
| `go` blocks, channel send/receive (`<-`) | Phase 3 | Phase 3 |
| `?` operator, `Ok` / `Err` / `Some` / `None` values | Phase 2 | Phase 2 |
| User-defined generics, traits, `impl` | not in v1 | "not supported in v1" |
| `unsafe extern` blocks, FFI declarations | Phase 5 | Phase 5 |
| `match`, pattern matching | Phase 2 (if in scope) | Phase 2 |
| Module system (`import "path"`, flat namespace; `pub` accepted, no effect) | Phase 5 | Phase 5 |
| `mod` / `use` | not in v1 | "not supported in v1" |
| `defer` / `close`, resource handling | later phase | "not in v1 core" |

`Result<T, E>` / `Option<T>` **types** parse in annotations and signatures
(the generic type syntax is shared), but constructing or consuming their
values and using `?` is rejected: specified in §11, implemented in Phase 2.

## 2. Lexical structure

Source files end in `.gzv`, are UTF-8, and use `//` line comments (no block
comments). A first-line `#!` is not special: `#` is not a valid character
(diagnostic suggests `//`). Newlines are insignificant except inside string
literals and to terminate `//` comments. Statements are terminated by `;`
(§3.3), never by newlines.

### 2.1 Keywords

```
fn let mut if else while for in return break continue struct
true false go chan select defer close unsafe extern match
import mod use pub trait impl const static as loop self
int float string bool unit Array Map Range Result Option
Some None Ok Err
```

**[Proposal]** All of the above are reserved keywords, including type names
(`int`, `Array`, …) and future constructs. Rationale: one reserved set keeps
the lexer total and lets the parser give phase-naming diagnostics instead of
confusing "expected expression" errors. Cost: these words cannot be used as
identifiers. Reopen if: dogfooding shows a keyword (e.g. `close`, `as`)
is needed as an ordinary identifier.

Identifiers: `[A-Za-z_][A-Za-z0-9_]*`, excluding keywords. Convention (not
enforced): `snake_case` bindings/functions, `CamelCase` structs.

### 2.2 Tokens (full v1 set)

```
// Single-character tokens
(  )  {  }  [  ]  ,  :  ;  .  +  -  *  /  %  =  <  >  !  ?  @

// Multi-character operators
==  !=  <=  >=  &&  ||  ->  =>  <-  ..

// Literals: integer, float, string ("..."), true, false; identifiers
```

The lexer produces the **full v1 token set**, including tokens the core
parser rejects (`<-`, `?`, `=>`, `chan`, `go`, …). A character outside this
set is a lexer error, never a panic. Common cases carry suggestions (§9.3):
`&` suggests `&&`; `|` suggests `||`; `'` suggests `"`; `#` suggests `//`.

### 2.3 The `<-` disambiguation rule

**[Proposal]** The lexer applies maximal munch with one explicit rule: when
`<` is **immediately** followed by `-` (no whitespace between), the two
characters lex as the single token `<-` (channel operator). Otherwise `<`
lexes alone. Consequences, all intended:

- `ch <- val` → send; `<-ch` → receive (distinguished by the parser by
  position: prefix `<-` is receive, infix `<-` is send; both rejected in
  Phase 1 with a Phase 3 diagnostic).
- `a < -b` (whitespace between) → `a < (-b)`: less-than with a negated
  operand. This is the documented escape hatch for writing `<` next to a
  negation.
- `a<-b` (no spaces) → send. There is no "less-than negative" reading
  without the separating space; the diagnostic for a rejected channel
  construct explains this.

Rationale: one lexical rule, no parser lookahead hacks, and channel code
keeps its `ch <- val` / `<-ch` shape from the D2/D3 design. Reopen if: the
Phase 2 checker wants different recovery behavior.

### 2.4 Number and string literals

- Integers are 64-bit signed (`i64`), decimal with optional `0x` hex and
  `_` separators (`1_000`, `0xFF`). A leading `0` is decimal, not octal.
- Floats are 64-bit (`f64`): digits with a `.` **plus at least one digit
  after the point** (`1.0`, `0.5`), or scientific notation (`1e3`, `2.5e-2`).
  **[Proposal]** `1.` (trailing dot, no digits) is not a float: it lexes as
  integer `1` followed by an unexpected `.` diagnostic. Rationale: this keeps
  `0..10` lexing as `0`, `..`, `10` with no backtracking. Reopen if: users
  demand trailing-dot floats.
- Range `0..10` is `..` (exclusive end); there is no `..=` in v1 core.
- Strings are double-quoted UTF-8, single-line, with escapes
  `\n \t \r \\ \"`. Unknown escapes and unterminated strings are lexer
  errors with spans. There are no multiline or raw strings in v1 core, and
  no `char` type or single-quoted literals (`'x'` suggests `"x"`).

## 3. Syntax (core subset)

EBNF notation: `|` alternative, `*` zero-or-more, `+` one-or-more, `?`
optional, `( )` grouping, `UPPER` tokens, `lower` rules.

```ebnf
program     := item+ ;
item        := fn_decl | struct_decl ;
fn_decl     := annotation* "fn" IDENT "(" params? ")" "->" type block ;
params      := ("mut"? IDENT ":" type) ("," "mut"? IDENT ":" type)* ","? ;
struct_decl := annotation* "struct" IDENT "{" (IDENT ":" type ",")* ","? "}" ;
annotation  := "@" IDENT ("(" annotation_arg* ")")? ;

block       := "{" stmt* expr? "}" ;            (* trailing expr = value *)
stmt        := let_stmt ";" | assign_stmt ";" | expr_stmt ";" | return_stmt ";"
             | break_stmt ";" | continue_stmt ";" | while_stmt | for_stmt ;
let_stmt    := "let" "mut"? IDENT (":" type)? "=" expr ;
assign_stmt := place "=" expr ;                (* place: IDENT | expr "." IDENT | expr "[" expr "]" *)
return_stmt := "return" expr ;
break_stmt  := "break" ;
continue_stmt := "continue" ;
while_stmt  := "while" expr block ;
for_stmt    := "for" IDENT "in" expr block ;
expr_stmt   := expr ;

expr        := or_expr ;
or_expr     := and_expr ("||" and_expr)* ;
and_expr    := eq_expr ("&&" eq_expr)* ;
eq_expr     := cmp_expr (("==" | "!=") cmp_expr)* ;
cmp_expr    := add_expr (("<" | "<=" | ">" | ">=") add_expr)* ;
add_expr    := mul_expr (("+" | "-") mul_expr)* ;
mul_expr    := unary_expr (("*" | "/" | "%") unary_expr)* ;
unary_expr  := ("-" | "!") unary_expr | postfix_expr ;
postfix_expr:= primary_expr (call_suffix | field_suffix | index_suffix)* ;
call_suffix := "(" (expr ("," expr)* ","?)? ")" ;
field_suffix:= "." IDENT ;
index_suffix:= "[" expr "]" ;
primary_expr:= INT | FLOAT | STRING | "true" | "false" | "(" ")"
             | IDENT | "(" expr ")"
             | "if" expr block ("else" ("if" expr block | block))?   (* else-less if: statement only, §5.5 *)
             | "fn" "(" params? ")" "->" type block     (* anonymous closure *)
             | IDENT "{" field_init ("," field_init)* ","? "}"   (* struct literal *)
             | "[" (expr ("," expr)* ","?)? "]"          (* array literal *)
             | range_expr ;
field_init  := IDENT ":" expr ;
range_expr  := add_expr ".." add_expr ;         (* binds tighter than ==, looser than + *)
type        := IDENT ("<" type ("," type)* ">")? ("[" "]")? ;
```

Notes and proposals:

- **[Proposal, amended per review: unit type added]** Every function
  declares an explicit return type; there is no inference of return types
  ("no type inference beyond literals" per the roadmap). The unit type is
  named `unit` and its only value is the literal `()`. Functions that
  return nothing declare `-> unit` and never need a dummy return value:
  ```gzv
  fn greet(name: string) -> unit {
      println(name);
  }
  ```
  The entry point is exactly `fn main() -> int` with zero parameters; the
  process exit code is main's return value (§10). A function body is a
  block: `return expr;` returns early, and a trailing expression without
  `;` is the implicit return. Falling off the end of a `-> unit` function
  returns `()`; falling off the end of any other function without returning
  is a runtime panic (`E0212`). Rationale: return types are the Phase 2
  checker's anchor points; explicit `unit` keeps procedures honest without
  dummy values. Reopen if: Phase 2 wants `main` to return `Result`.
- **[Proposal, amended per review: else-required rule added]** `if` with
  `else` is an **expression** (both branches evaluate to a value; `else if`
  chains allowed); `if` **without** `else` is a **statement only** — it may
  not appear in expression position (operands, call arguments, `let`
  initializers, `return` values, array/struct literals, block tails), where
  the parser rejects it (`E0107`). `while`/`for` are **statements**; loops
  and else-less `if` evaluate to the unit value `()`, which is only
  observable if explicitly returned from a `-> unit` function.
  Rationale: matches the roadmap ("loops are statements"), keeps
  conditional values ergonomic, and an else-less expression would have no
  defined value. Reopen if: checker scoping prefers statement-only `if`.
- **[Proposal]** Blocks are expressions: `{ stmts...; tail }` evaluates to
  `tail`; a block of only statements evaluates to the unit value `()`.
  Assignment (`=`) is a **statement**, not an expression, so `=` vs `==`
  confusion cannot hide inside larger expressions. Reopen if: Phase 2 wants
  `let x = (y = 1);` rejected at parse rather than by grammar shape.
- Precedence, lowest to highest: `||`, `&&`, `== !=`, `< <= > >=`,
  range `..`, `+ -`, `* / %`, unary `- !`, postfix call/index/field.
  All binary operators are left-associative. `..` sits between comparison
  and addition so `0..n+1` parses as `0..(n+1)` and `a < b..c` is a parse
  error rather than a surprise.
- Type syntax (Phase 1): a name with optional generic arguments
  (`Array<int>`, `Map<string, int>`, `Result<int, string>`, `chan<int>`,
  `Range`). Types parse but are **not checked** in Phase 1.
- Struct literals use the struct name (`Point { x: 1, y: 2 }`), so they
  never clash with blocks. Field order in the literal is free; missing or
  unknown fields are runtime panics (`E0205`).

### 3.1 Entry point and program shape

A program is one file, a sequence of items. Top-level statements are not
allowed. Execution starts at `fn main() -> int`. Duplicate top-level names
(`main` twice, two structs called `Point`) resolve to the **last**
definition in Phase 1 (documented leniency; Phase 2 makes duplicates a
static error).

Multi-file programs (Phase 5) use path imports: a top-level
`import "path";` statement pulls the other file's items into the single
flat top-level namespace (no nested modules). Import paths resolve
against the importing file's directory; an already-loaded file loads
once (diamond imports are fine). `pub` may prefix any top-level item
and has no effect — there are no private items. `mod` and `use` are not
in v1. An `import` in statement or expression position is a parse error
(`E0157`); an `import` that cannot be resolved is a static error
(`E0401`: single-file mode, or the file is missing). Duplicate
top-level names across imported files are static errors (`E0311`, as
for duplicates in one file, reported at the later file); a file that
imports itself, directly or through other files, is a static error
(`E0402`, circular import, reported at the import that closes the
cycle). Diagnostics name their origin file (`docs/project.md`).

### 3.2 Semicolons

`;` terminates `let`, assignment, expression-statement, `return`, `break`,
and `continue`. Blocks, `if`/`while`/`for`, function and struct declarations
take no trailing `;`. A missing `;` is a parse error pointing at the next
token ("expected `;`"). A block's trailing expression **omits** `;`
(`{ …; tail }` returns `tail`; `{ …; tail; }` returns `()`).

### 3.3 Out-of-subset rejections

The parser recognizes out-of-subset constructs far enough to reject them by
name. Minimum required rejections (negative corpus, §9.4):

- `go <block>` and any use of `<-` → "channels and `go` blocks execute in
  Phase 3" (code `E0151` / `E0152`). These rejections retire as Phase 3
  enables the constructs; semantics are specified in §13.
- Postfix `?` (`expr?`) → **retired in Phase 2**: `?` now parses and
  executes (§11, §12); the `E0153` rejection no longer exists.
- `fn name<T>(…)` or `struct Name<T> …` (user generics) → "user-defined
  generics are not supported in v1" (`E0154`).
- `unsafe` blocks / `extern` blocks or declarations → "FFI executes in
  Phase 5" (`E0155`).
- `match` → "pattern matching is a Phase 2 concern" (`E0156`).
- `mod` / `use` → "not supported in v1 (multi-file programs use
  `import "path"`)" (`E0157`). `import` in statement or expression
  position → "`import` is only allowed at the top level" (`E0157`).
  Top-level `import "path";` and `pub` are core (Phase 5, §3.1).
- `Ok(…)`, `Err(…)`, `Some(…)`, `None(…)` → **retired in Phase 2**: these
  are value constructors that parse, check, and execute (§11, §12); the
  `E0206` runtime rejection no longer exists.
- `x.y(…)` (method-call syntax) is rejected by the parser: Toto v1 has
  **functions only, no methods and no method-call syntax** — call a function
  with the value as an argument instead (`E0158`). Plain field access
  `x.y` remains core.

## 4. Types (Phase 1 reading)

Runtime values carry tags; static types are documentation until Phase 2.

| Type | Values | Notes |
|---|---|---|
| `int` | 64-bit signed | checked arithmetic, §5.1 |
| `float` | 64-bit IEEE 754 | NaN/±inf per IEEE, §5.4 |
| `string` | immutable UTF-8 | value semantics via sharing, §5.3 |
| `bool` | `true` / `false` | strict conditions, no truthiness, §5.5 |
| `Array<T>` | mutable ordered sequence | reference semantics, §5.3 |
| struct types | named fields, declaration order | reference semantics, §5.3 |
| `Range` | `{ start: int, end: int }` | from `..`, iterated by `for` |
| function/closure | code + captured environment | identity equality only |
| `unit` | the single value `()` | procedures' return type; what statements evaluate to |

`Map<K,V>`, `Result<T,E>`, `Option<T>`, `chan<T>` parse as types; no values
of these types can be produced in Phase 1.

## 5. Semantics

### 5.1 Evaluation order

**[Proposal]** Strict left-to-right, defined per form (all **[Proposal]** as
a set; the PDF fixes nothing here):

- Binary operator: evaluate left operand, then right operand, then apply.
- Call: evaluate the callee expression, then arguments left to right.
- Array literal: elements left to right. Struct literal: fields in source
  order. Index: base, then index. Field access: base.
- `let name = e;`: evaluate `e` fully, then bind. Assignment `p = e`:
  evaluate `e`, then store into `p`.
- `&&` / `||` short-circuit: right side evaluates only if needed.

Rationale: observable through `print` and mutation (`push`), so the order
must be fixed now — differential testing in Phase 4 compares against this
interpreter. C-style unspecified order is rejected. Reopen if: a backend
proves left-to-right blocks a mandatory optimization (unlikely for v1).

### 5.2 Integer arithmetic: overflow and division by zero

Per ratification: all `int` operations (`+ - * / %`, unary `-`) are
**checked**; overflow, division by zero, and `%` by zero **panic** with the
operator's span (`E0201`, `E0202`). `i64::MIN / -1` panics. `/` truncates
toward zero; `%` takes the dividend's sign (Rust semantics). Explicit
wrapping operations are TBD — no wrapping builtins exist in Phase 1.
`parse_int` panics on malformed input or out-of-range values (`E0210`).

### 5.3 Value vs reference semantics

**[Proposal]** Scalars (`int`, `float`, `bool`) are copied on assignment,
argument passing, and capture. Strings are immutable; assignment and passing
share the underlying buffer, which is unobservable, so strings behave as
values (`==` compares contents). Arrays, structs, and ranges' containers
are **references**: assignment, passing, and capturing share the object, and
mutation through one handle is visible through all handles:

```gzv
let a = [1, 2];
let b = a;        // shares the array
push(b, 3);       // visible through a: a == [1, 2, 3]
```

`let` is **binding immutability only** (per D3): `let a = [1]; a[0] = 2;` and
`p.x = 1;` through a `let`-bound struct are legal; rebinding (`a = …`) needs
`mut`. Rationale: matches D3's "let does not make a struct's fields
immutable", keeps the interpreter's value model (reference-counted
containers) honest, and defers borrow checking to Phase 2. Reopen if: Phase 2
wants deep immutability for `let`.

### 5.4 Equality

**[Proposal]**

- `==` / `!=` require both operands to have the **same runtime type**;
  mixed-type comparison is a runtime panic (`E0203`), except `int` vs
  `float`, which panics with a diagnostic suggesting explicit conversion
  (`to_float` / `to_int`). Rationale: silent cross-type `false` hides bugs;
  Phase 2 will flag these statically, and the interpreter must not bless
  them now. Reopen if: dogfooding wants total cross-type equality.
- Same-type rules: `int`/`bool`/`unit` by value (`() == ()` is `true`);
  `float` by IEEE 754, so `nan == nan` is `false` and `0.0 == -0.0` is
  `true` (use `is_nan` to test);
  `string` by contents; `Range` by endpoints; `Array`/struct **structural
  (deep)** equality with cycle detection — cyclic structures fall back to
  identity comparison rather than recursing forever; functions/closures by
  **identity** (same value, not same code);
  channels compare equal to every channel (`c == c` and `c1 == c2` are
  both `true`; `!=` is `false`) — channel identity is not observable
  through equality. Pinned by `conc19_chan_eq`; all executors agree
  (recorded Phase 5, divergence disposition).
- Ordering `< <= > >=`: `int`, `float` (NaN comparisons are `false`, per
  IEEE), and `string` (lexicographic byte order). Other types are a runtime
  panic.
- Note: Phase 2 will reject cross-type `==` statically, so the runtime
  panic above becomes unreachable in checked programs; it exists in Phase 1
  because the interpreter runs unchecked code.

### 5.5 Conditions, truthiness, and control flow

**[Proposal]** No truthiness: `if` and `while` conditions must be `bool`;
anything else is a runtime panic (`E0207`). `for x in e`: `e` must be a
`Range` (iterates `start..end`, empty when `start >= end`) or an `Array`
(iterates elements in order); anything else (including strings — there is no
`char` type) is a runtime panic (`E0207`). `break` exits the innermost loop;
`continue` starts its next iteration; both outside a loop are parse errors.
`break`/`continue` carry no values (loops are statements).

### 5.6 Bindings, mutability, shadowing

`let name [: type] = expr;` creates an immutable binding; `let mut` allows
rebinding by `=`. The annotation, when present, must parse; it is stored in
the AST and **ignored at runtime** in Phase 1 (Phase 2 enforces it).
**[Proposal]** Shadowing is allowed: a later `let` with the same name in an
overlapping scope hides the earlier one (Rust-like). Rationale: simplest
predictable block-scoping for a tree-walking interpreter; Phase 2 may warn.
Every `let` requires an initializer — there are no uninitialized bindings,
no `nil`, and no null in v1 core (`Option` covers absence from Phase 2).

### 5.7 Closures and capture

**[Proposal]** Closures are anonymous `fn (params) -> Ret { body }` values.
They capture their defining environment **by value at creation**: each
in-scope binding is read when the closure value is created and stored with
the closure. Scalars are copied; arrays/structs are shared by reference
(consistent with §5.3). Later rebinding of the outer variable does not
affect the closure; mutation of a captured array/struct through the closure
is visible outside (same object). Concretely: capture copies the binding's
value, and for arrays and structs that value *is* the shared reference, so
a closure created before a mutation observes that mutation:
```gzv
let a = [1];
let f = fn() -> int { return a[0]; };
a[0] = 9;
f()   // 9: f shares the same array object
```
Rebinding after creation, by contrast, is not observed: after
`let x = 1; let g = fn() -> int { return x; }; x = 2;`, `g()` still
returns 1. Named `fn` declarations do not capture —
they resolve names in the top-level scope at call time, which makes direct
recursion (`fact` calling `fact`) work. Parameters may be declared `mut`
(local rebinding only; no effect on the caller — all argument passing is by
value/share as in §5.3, never by reference).

Rationale for capture-by-value: it is the only rule that stays sound when
closures later cross green-thread boundaries (D2/D3: no `mut` capture by
reference), and it keeps the interpreter's environment model simple
(one flat captured map, no reference cycles through scopes except via
explicit containers). Reopen if: Phase 2 ergonomics demand capture-by-
reference with checker enforcement.

### 5.8 Calls, arity, and runtime failures

Calling a non-function value, calling with the wrong number of arguments,
indexing a non-array, accessing a missing field or index (out-of-bounds
access panics per D4), and reading an undefined variable are all **runtime
panics with spans** (`E0204`–`E0209`), never Rust panics. The interpreter
implements total internal error paths for every user-reachable failure; any
Rust panic on a user program is a bug.

### 5.9 Recursion depth

**[Proposal]** The **interpreter** limits the Toto call stack to 8,192
frames; exceeding it panics cleanly (`E0211`, "stack overflow", with span)
instead of overflowing the host stack. This limit is INTERPRETER-ONLY:
compiled code has its own stack limits (ratified 64 KiB stacks, Phase 3).
Rationale: "never a Rust crash" requires a defined limit; 8,192 fits real
programs and keeps debug-build stacks safe. Reopen if: measurement shows
real programs need more.

## 6. Panic contract

A panic is an unrecoverable failure for a bug (D4): overflow, division by
zero, out-of-bounds, failed assumption — never an ordinary error a caller
handles (`Result` is Phase 2). Format, all **[Proposal]**:

```text
panic[E0201]: integer overflow
 --> prog.gzv:7:12
  |
7 |     return x + 100000000000000000000;
  |                ^^^^^^^^^^^^^^^^^^^^
stack trace:
  in double at prog.gzv:7:12
  in main at prog.gzv:13:5
```

- First line: `panic[<code>]: <message>`, message in sentence case, no
  trailing period, naming values where helpful (`index 5 out of bounds
  for array of length 3`).
- Then the span snippet (` --> file:line:col`, source line, caret span).
- Then `stack trace:` with one `in <fn> at <file>:<line>:<col>` per live
  Toto frame, innermost first. Builtin frames appear as `in <builtin>`.
  Traces longer than 22 frames are truncated to the innermost 20 plus the
  outermost 2, with a `... (N frames omitted) ...` line between them
  (implementation amendment, Phase 1: full 8,192-frame traces are unreadable
  and untestable as goldens; the omission count keeps them exact).
- Exit code 1. (Static diagnostics exit 2; CLI/file errors exit 3; §10.)

## 7. Minimal builtins (the only stdlib in Phase 1)

Signatures use `T` informally (no user generics; builtins dispatch on
runtime tags). All IO failures **panic** in Phase 1 (`E0210`) — a deliberate
stopgap: `Result`-returning IO arrives in Phase 2, and the spec records here
that builtin signatures will gain `Result` returns then.

```text
print(x: any) -> int            stdout; returns bytes written
println(x: any) -> int          stdout + "\n"; returns bytes written
len(s: string) -> int           bytes? chars? — see below
len(a: Array) -> int            element count
split(s: string, d: string) -> Array<string>
trim(s: string) -> string       strips ASCII whitespace both ends
upper(s: string) -> string      ASCII upper
lower(s: string) -> string      ASCII lower
contains(s: string, sub: string) -> bool
to_string(int | float | bool) -> string
parse_int(s: string) -> int     panic on malformed/out-of-range
to_int(x: float) -> int         truncation; panic on NaN/inf/out-of-range
to_float(x: int) -> float        exact when |x| < 2^53, else nearest
is_nan(x: float) -> bool
abs(x: int) -> int              panic on i64::MIN
min(a: int, b: int) -> int
max(a: int, b: int) -> int
sqrt(x: float) -> float          NaN on negative (IEEE, no panic)
floor(x: float) -> float
ceil(x: float) -> float
push(a: Array, v: T) -> int     appends; returns new length
pop(a: Array) -> T              removes last; panic if empty
read_file(path: string) -> string    panic on IO error
write_file(path: string, contents: string) -> int  bytes written; panic on error
file_exists(path: string) -> bool
```

This catalog is FROZEN as of spec approval (review 2026-11-14). No builtin
may be added, removed, or re-specified during Phase 1; any change needs a
stop-and-ask with a spec amendment.

**[Proposal, decided per review: chars]** `len` on strings counts Unicode
scalar values (chars), not bytes, so `len("héllo") == 5`. Rationale: byte
counts surprise CLI authors; chars match user intent for a v1 builtin.
Reopen if: a byte-length builtin is needed for IO framing (add `byte_len`
then).

**[Proposal]** `print`/`println` accept any value and format it with the
`Display` rules: ints decimal; floats shortest round-trip (`1.0` prints as
`1.0`, not `1`); bools `true`/`false`; strings raw (no quotes); arrays
`[1, 2]`; structs `Point { x: 1 }`; ranges `0..10`; functions/closures
`<closure>` / `<fn name>`; unit prints as `()`. Rationale: dogfooding
needs `print(anything)`;
typed `print(string)` would force `to_string` everywhere. Reopen if: Phase 2
wants `print` restricted to strings.

## 8. Annotations

`@name` or `@name(args)` before `fn`/`struct` items, parsed into the AST
and ignored by the interpreter. No builtin annotations exist in Phase 1;
unknown annotations are silently kept (Phase 2 may assign meaning, e.g.
test markers). Commas separate args; args are literals or `key = literal`.

## 9. Diagnostics from day one

### 9.1 Format and codes

```text
error[E0102]: expected `;` after let statement
 --> prog.gzv:3:12
  |
3 | let x = 1
  |            ^ help: add `;` at the end of the statement
```

Every diagnostic carries: stable code, `error`/`panic` kind, message,
`file:line:col` span, source snippet with caret span, and optional
`help:` suggestion. Codes: `E00xx` lexer, `E01xx` parser, `E02xx` runtime
(panics), `E03xx` static type errors (Phase 2, §12), `E04xx`
project/module errors (Phase 5: `E0401` unresolved import, `E0402`
circular import, §3.1), `E06xx` package resolution (Phase 7,
Stage 3, §16). The code list lives
with the implementation and is appended to
this spec when new codes are added (spec amendment, not silent drift).

### 9.2 Multi-error recovery

The parser reports **all** recoverable syntax errors in one run (minimum
demonstration: one file with 3 errors yields 3 diagnostics), synchronizing
at statement boundaries (`;`, `}`, item starts), capped at 16 diagnostics
per file before stopping with "too many errors". The lexer likewise collects
per-file errors and continues after unexpected characters. Runtime panics
abort on the first panic (no multi-error at runtime).

### 9.3 Suggestions (minimum three; all **[Proposal]** in wording)

1. `=` vs `==`: `if x = 1 {` → "did you mean `==`? assignment is a
   statement and cannot appear inside an expression".
2. Unclosed delimiter: reports the **opening** location —
   "unclosed `{` opened here".
3. Single-quoted `'x'` → "strings use double quotes: `\"x\"`".
4. `# comment` → "comments start with `//`".
5. Lone `&` / `|` → "did you mean `&&` / `||`?".
6. `loop {` → "use `while true {` in v1 core".
7. Missing `;` → points at the next token with the fix.

### 9.4 Negative corpus rule

Every invalid program in the negative corpus fails with a readable
diagnostic and exit code 2 (static) or 1 (runtime panic). A Rust panic or
crash on any corpus program — including adversarial inputs (deep nesting,
huge literals, cyclic arrays via `push`, 8,193-deep recursion) — is a bug.
The corpus runner treats a Rust panic (nonzero signal, `panicked` on
stderr from the test harness itself) as a test failure.

## 10. CLI

- `toto run <file.gzv>` — lex, parse, **check** (§12), then execute from
  `main`. Any static diagnostic (lex, parse, or type error) exits 2 and
  nothing executes. Exit code: main's return value (must be
  0–255, else panic exit 1); runtime panic exits 1 with the §6 format;
  static diagnostics exit 2; missing/unreadable file or bad CLI usage
  exits 3 (`toto: couldn't read <file>: <os error>`). A normal `main`
  return of 1 is indistinguishable from the panic exit code 1; this
  collision is accepted as standard practice, not an error.
- `toto --help` — usage text, exit 0. `toto --version` — `toto 0.1.0`,
  exit 0. **[Proposal]** Initial version `0.1.0` for the CLI crate.
- Single-file programs only; any `import` is a Phase 2 diagnostic (§3.3).
  No REPL, no formatter, no module search paths in Phase 1.

## 11. Result / Option / `?` (specified now, implemented in Phase 2)

Syntax (reserved, parses): `Result<T, E>` and `Option<T>` types;
`?` postfix on expressions; `Ok(e)`, `Err(e)`, `Some(e)`, `None` value
forms. Semantics (binding on Phase 2):

- `Result<T, E>` and `Option<T>` are built-in generics (D5); the
  `Result<T, Error>` simplification is **not** adopted.
- `?` propagates errors, converting via the built-in conversion to the
  function's error type / the standard `Error` type (message, code, cause
  chain), **preserving the original error as cause** — no lossy
  stringification (D4 rule). The conversion is hardcoded in v1 (no traits).
- Unused `Result` values are a compile error (Phase 2 checker rule, not an
  interpreter rule).
- `panic` is only for bugs; a panic in a child task fails its structured
  scope by default (Phase 3 failure policy).
- Implemented in Phase 2: checking and execution per §12. The §11
  `E0153`/`E0206` rejections are retired: `?`, `Ok`, `Err`, `Some`, `None`
  now parse, check, and execute.

## 12. Static typing (Phase 2)

Pipeline: lex → parse → **check** → interpret. `toto run` checks first;
a program with any `E03xx` diagnostic never executes (exit 2, §10). The
tree-walking interpreter still runs unchecked code in the sense that it
performs its own runtime tag checks, but every program it runs has passed
the checker, so the `E0203` cross-type `==` panic (§5.4) is unreachable in
checked programs and survives only as a backstop. No checked program can
reach it: `==`/`!=` with mismatched static types is `E0303`.

### 13.1 What the checker enforces

- Function boundaries are explicit: every parameter and the return type
  carry an annotation (§5.6). There is no inference across function
  boundaries and no Hindley-Milner anywhere.
- `let` types come from initializers; an annotation, when present, must
  match. Rebinding a `let` (non-`mut`) binding is `E0313`; shadowing by a
  later `let` is allowed (§5.6).
- Struct literals supply every declared field exactly once, each with the
  declared type; unknown or duplicate fields are errors. Field and index
  targets resolve against the struct declarations and array element types.
- Conditions are strictly `bool`; expression-`if` branches must agree in
  type; `return` must match the declared return type; `for` iterates
  `Range`/`Array` only. Arithmetic, ordering, and equality follow the
  §5.2/§5.4 matrices exactly; cross-type `==` is rejected statically.
- Calls check arity and argument types. The frozen 25 builtins (§7) check
  by fixed signatures; the catalog does not grow. Builtins are callable by
  name only — a bare builtin in value position is `E0305`, as is calling a
  non-function value.
- Duplicates (functions, structs, parameters, struct fields) are `E0311`;
  the entry point must be exactly `fn main() -> int` (`E0312`).
- D4: an unused `Result` value in statement position is `E0307`. (An
  unused `Option` is legal: it is not a result.)
- `?` unwraps `Result<T, E>` to `T` (or `Option<T>` to `T`) and
  early-returns the error/None from the enclosing function. The enclosing
  function must return a matching `Result`/`Option`. The v1 error
  conversion (§11, D4) is defined only for **identical** error types —
  identity propagation, which preserves the original error as cause
  trivially. `?` across differing error types is `E0308`: handle the
  error explicitly. There is no `Error` supertype and no other conversion
  in v1.
- Constructors need context, never guessing: `Ok`/`Err` require an
  enclosing `Result<T, E>` annotation (parameter, return, `let`, or
  another `Result`-typed position); `None` requires `Option<T>` context;
  an empty array `[]` requires an `Array<T>` annotation. Context does not
  flow inward through array literals: `[Ok(1)]` is still `E0309` — bind
  the element first. (If this proves unergonomic, it reopens here, not in
  the checker alone.)
- Types: `int float string bool unit Range Array<T> Map<K,V> Result<T,E>
  Option<T>`, struct types, and function types spelled `fn(T1, T2) -> R`.
  The `fn(…) -> …` spelling is **provisional**: the core subset has
  first-class functions but no annotatable function-type syntax, so a
  minimal spelling was chosen to keep higher-order functions checkable
  (e.g. the `apply` corpus program). Confirm or rename — the spelling is
  isolated to the type parser. `Map<K,V>` checks as a type but no values
  exist yet (constructors are a later phase). A struct cannot reference
  itself, directly or through another struct: field types must resolve
  against already-declared structs, so recursive types are rejected with
  `E0302`. Closures capture per §5.7; the checker records each closure's
  capture list (names bound outside the closure that its body uses) in
  HIR for MIR closure conversion. Named `fn` declarations do not capture.

### 13.2 Type-error codes (E03xx)

All `E03xx` diagnostics use the §9.1 format with `error` kind and name
expected vs found types. Multi-error recovery is preserved: checking
continues after an error where the scope structure allows, so one run can
report several independent type errors (at most 16 per file, as in §9.2).

| Code | Meaning |
|---|---|
| `E0301` | undefined variable (no binding, function, or builtin in scope) |
| `E0302` | unknown or misapplied type (bad arity, recursive struct) |
| `E0303` | type mismatch (expected vs found, incl. cross-type `==`) |
| `E0304` | wrong number of arguments |
| `E0305` | bad call (non-function callee; bare builtin value) |
| `E0306` | cannot iterate (`for` over non-`Range`/`Array`) |
| `E0307` | unused `Result` value |
| `E0308` | bad `?` (non-`Result`/`Option` operand, wrong enclosing return, error-type mismatch) |
| `E0309` | cannot infer (empty array, `None`, `Ok`/`Err` without context) |
| `E0310` | bad struct/array target (missing/unknown/duplicate field, field of non-struct, index of non-array) |
| `E0311` | duplicate definition (function, struct, parameter, field) |
| `E0312` | bad or missing `main` |
| `E0313` | illegal rebinding (`let` reassigned, function assigned) |
| `E0314` | D3 ownership violation (`go` captures `mut`, use after send-move) |
| `E0315` | `close` on a channel with no zero value (`chan`/`fn` element) |

## 13. Concurrency (Phase 3)

Amendment record (2026-11-14, Phase 3 kickoff): the six semantics
decisions below were specified explicitly by the language owner to
resolve the Phase 3 spec-silence blocker (channel construction, scopes,
select, close, cancellation, `go` handles). Each is binding. Points
marked **[Proposal]** are sub-details neither the PDF nor the owner
fixed; they follow the Appendix A review process (flagged here, owner
confirms or corrects — never silent).

### 13.1 Channels

- Type `chan<T>` (§4). Values are created by builtins: `chan<T>()`
  (unbuffered) and `chan<T>(n)` (bounded, capacity `n`). `n` must be an
  `int`; a negative capacity panics (`E0204`) **[Proposal]**.
- The builtin catalog grows 25 → 27: `chan`, `close`, `closed` are added
  (mandated by this section; §7's freeze is otherwise unchanged). No
  other channel builtins exist in v1 (`len` does not apply to channels).
  (Correction 2026-11-17: 27, not 28 — the old total double-counted
  `len`; verified against `BUILTIN_NAMES`.)
- Send is `ch <- v`, receive is `<-ch` (§2.3). Send blocks when a bounded
  channel is full, and on an unbuffered channel until a receiver takes
  the value; receive blocks when the channel is empty. `select` with
  `default` (§13.3) is the non-blocking path.
- Ownership (D3, brief-mandated): a `go` block that captures a `mut`
  binding by reference is a static error; `ch <- x` where `x` is a `mut`
  binding moves `x` (the sender loses access — any later use is a static
  error). Both are code `E0314` with a message naming the violated rule.
  Deep immutability beyond this is by construction (value capture copies
  scalars per §5.7; containers share) with the Phase 4 race detector as
  backstop — the checker does not attempt whole-program alias analysis.
- `close(ch)` marks a channel closed. Send on a closed channel panics;
  double-close panics (both `E0213` **[Proposal]**). Receive on a
  closed-and-drained channel yields the zero value of `T`; buffered
  items are received first. `closed(ch)` reports whether `close` was
  called (independent of drain state).
- Zero values **[Proposal]**: `int` 0, `float` 0.0, `string` `""`,
  `bool` false, `unit` `()`, `Array<T>` `[]`, `Range` `0..0`,
  `Option<T>` `None`, `Result<T,E>` `Ok(zero of T)`, struct `S` has each
  field zeroed (well-founded: recursive types are rejected, E0302).
  `Map` has no values, so a `Map` zero never arises. `chan` and `fn`
  element types have no zero value: `close` on `chan<chan<T>>` or
  `chan<fn…>` is a static error (`E0315`) **[Proposal]**.

### 13.2 Tasks and scopes

- `go <block>` is a statement yielding `unit`. Completion is observed
  through scope waits and channels only; there are no handles and no
  join operation.
- Every `go` block is a child scope of the scope executing it; `main`
  is the root scope. A scope completes when all its children complete;
  `main` returns (and the process exits) only after all descendants
  complete. Results flow through channels, not return values.
- A child panic fails its scope (§11): siblings suspended in
  send/receive/select are released via the cancellation unwind (§13.4);
  tasks at non-suspending points run on to their next suspension
  boundary or completion (cooperative — no preemption). After all
  children settle, the scope re-panics with the first child's panic,
  preserving its span and trace **[Proposal: first-panic-wins,
  settle-then-propagate]**.
- Deadlock (PDF failure policy): all tasks suspended with none runnable
  and no pending wakeups (no timers or IO exist in v1) is a runtime
  panic reporting deadlock with the suspended tasks' spans **[Proposal]**.
  Task leaks are impossible by construction (scopes always wait);
  orphaned tasks in soak tests are a bug.

### 13.3 `select`

- Statement form with one or more guarded cases and an optional
  `default` (which, if present, must be last **[Proposal]**). An empty
  `select {}` with no cases blocks forever (Go package).
- Guard forms: `let v = <-ch` (receive and bind) **[Proposal: binding
  surface]**; `<-ch` (receive and discard); `ch <- expr` (send).
  Guards are checked like their statement forms (channel types, send
  value matches `T`). At most one `default`; malformed `select` is a
  parse error (`E0159`).
- Semantics (owner-specified): the first ready case runs; choice among
  simultaneously ready cases is random. With `default`, the statement
  never blocks. Random tie-breaks use the scheduler RNG, which is
  seeded in deterministic test mode (brief §2).

### 13.4 Cancellation

- Trigger in Phase 3: child panic failing its scope, only. No explicit
  cancel operation exists (parked-recv cancellation is Phase 4).
- Mechanism: a task suspended in send/receive/select when its scope
  fails is released by panicking at the blocked operation's span
  (`E0214` **[Proposal]**, message names cancellation). Tasks that
  complete without suspending again are unaffected; a compute-bound
  sibling delays failure propagation until it suspends or completes
  (documented cooperative limitation).

## 14. Declared state + migration (Phase 6)

Amendment record (2026-11-17, Phase 6 authorization): the rules below
pin authorization amendments §2.1–2.7 and §2.15. Single-generation
semantics (init, read, write through all executors) land with this
amendment; reload-time enforcement (migration invocation, matrix,
queued-value validation) is Stage 3. Until Stage 3, a reload
re-initializes declared state exactly like all other program state
(drain-and-restart, §14.1) — never silently preserved, never silently
reinterpreted. Production builds (`toto build`) never hot-reload.

### 14.1 The `state {}` block

- Syntax: `state { name: Ty = init; ... name: chan<T>; ... }` — a
  top-level item, at most ONE per program across all files (the flat
  namespace makes the program the unit). A second block is static
  error `E0316` (merge the bindings into one block). A `state {}`
  block in any non-top-level position is error `E0101` (top-level
  items only); a struct literal named `state` still parses as an
  expression (the placement error fires only when the brace cannot
  open a struct literal).
- `state` and `migrate` are CONTEXTUAL keywords: `state` is special
  only as a top-level item followed by `{`; `migrate` only
  immediately before `fn` at top level. Everywhere else both are
  ordinary identifiers. Corpus check (2026-11-17): no existing program
  uses either as an identifier in those positions — zero breakage.
- Each binding is implicitly mutable program state (no `mut`
  keyword; `mut` is implied). Reassignment follows ordinary
  assignment rules; a local shadowing a state name uses the local
  (§3.2), and `go` blocks may freely read and write state (shared
  cells, not `mut` captures — D3's capture rule is untouched).
- State names share the flat value namespace: a collision with any
  other top-level value name (`fn`, `extern`, another state binding,
  a migrate name) is `E0311` — the existing duplicate-name error, no
  new code. A state name may match a STRUCT name (values and types
  are separate namespaces, as for locals).
- Initializers evaluate in declaration order at load, before `main`.
  An initializer sees only builtins and EARLIER state bindings; a use
  of a later binding (or of itself, including inside a closure in the
  initializer) is `E0319` (run order is declaration order). This
  includes deferred code — the Rust parallel (declaration-before-use
  holds inside closures).
- Data bindings require an initializer (`E0317` without one).
  Initializers are checked as ordinary expressions with no lexical
  bindings in scope. Banned in initializers (`E0320`): user-function
  calls (the callee's view of state is unordered at load) and channel
  operations that can block (`<-`, `ch <- v`, task spawn). (`?` needs
  no ban: with no enclosing function it already fails as E0308.) IO
  builtins are allowed (they
  run once at load, deterministically for a fixed environment).
  Channel construction nested in an initializer routes the program to
  the runtime executor (`uses_concurrency`).
- Channel bindings MUST be bare (`name: chan<T>;`); an initializer
  on one is `E0318`. Any `chan` element type is accepted at
  declaration; channel capacity at declaration is always unbuffered
  (bounded channels arise only from explicit `chan<T>(n)` calls).
- Any type may be held (including `fn` values); the reload whitelist
  (channels + plain data, §14.3) governs preservation, not
  declaration. Sending a non-deeply-immutable state binding over a
  channel is `E0314` (reads alias the shared cell, like field/index
  reads); deeply immutable state sends normally.
- Execution: slots are assigned in declaration order — the order IS
  the cross-executor contract (tree-walker, MIR executor, JIT agree
  slot-for-slot). Initialization is prepended to `main`'s entry
  (`StateSet` per binding, `ChanNew` + `StateSet` for channels).
  Reads/writes lower to `StateGet`/`StateSet`. State-held heap objects
  join every GC root set (threshold, explicit, and rendezvous
  collections), so live state is never swept under a running program.
- Concurrency (as implemented; recorded 2026-11-17): each
  `StateGet`/`StateSet` is atomic with respect to other state
  accesses — single-threaded executors serialize everything, and
  task mode clones/stores one slot under the shared-state mutex, so
  no torn slots. Compound sequences are NOT atomic: concurrent
  read-modify-write from multiple workers races (last-writer-wins).
  Coordinate through channels (both concurrent corpus programs are
  single-writer rendezvous chains).
- Reloads (until Stage 3): declared state re-initializes from scratch
  in the new generation. This is drain-and-restart for state —
  visible and total, consistent with all other program state — and
  Stage 3 upgrades it to preservation-through-migration per §14.3.

### 14.2 `migrate` functions

- Syntax: `migrate fn S(old: S) -> S { ... }` — a top-level item
  naming no new namespace: the migrate NAME shares the flat value
  namespace (`E0311` on collision) but is never callable — any
  reference or call is `E0325` (migrate functions run at reload,
  never as ordinary calls). The declared name is not required to
  equal `S` (lookup is by parameter type; verified against
  `HMigrate`/reload lookup, recorded 2026-11-17).
- Exactly one parameter (`E0321` otherwise); its type must be a
  struct `S` (`E0322` otherwise); the return type must equal `S`
  (`E0323` otherwise — no implicit conversion, no implicit field
  defaults: there is NO implicit-default mechanism in v1, so any
  added/removed/renamed field or field-type change needs a migrate,
  and without one the reload is an atomic restart, loudly).
- At most one migrate per struct (`E0324` on the second).
- Determinism (`E0325`): the body must be a closed pure expression
  over the old value. Banned: user-function calls (including deferred
  calls inside closures — strict, relaxable), IO and channel builtins
  (`print`, `println`, `read_file`, `write_file`, `file_exists`,
  `chan`, `close`, `closed`), foreign calls, channel operations,
  task spawn, `select`, and references to declared state (input is
  only the old struct). (`?` needs no ban: with a struct return type
  it already fails as E0308.) Pure builtins (`len`, string/math/array
  ops, struct construction, field reads) are allowed.
- Totality (`E0326`): every field of `S` must be read from the
  parameter (direct `old.field` reads, anywhere in the body including
  nested closures; shadowing the parameter name breaks the chain and
  fails totality). Returning the parameter itself (`return old;` or a
  trailing `old`) is trivially total. Rationale: migration must
  never silently drop data; dropping a field intentionally is not
  expressible — the reload then restarts instead (loud, §14.3).
- First-load checking validates well-formedness plus totality against
  the CURRENT definition of `S`. Reload time re-checks totality
  against the OLD generation's `S` and runs the function (Stage 3).
  The body is lowered with the program but never executed during
  single-generation runs.

### 14.3 Reload contract (enforced in Stage 3; stated here)

- Retention is by GC reachability (`Heap::is_live` + the
  reference→module side table), NOT epoch tags — within D7's
  "epoch or reference-based reclamation". Reference-based
  reclamation plus name-relink plus generational-handle staleness
  plus name-based struct snapshots cover every stale vector (Stage 2a
  proof, `docs/phases/phase-6-state-preserving-reload.md`).
- Struct snapshots are name-based; layout comparison is a
  tag-mismatch discipline applied BEFORE any field is touched: a
  struct whose layout changed without a migrate (or whose migrate
  fails reload-time checks or panics) routes to loud atomic restart —
  never silent reinterpretation, never a silent skip.
- Channel-type revalidation: an empty queue with a changed element
  type carries with the new type; a NON-empty queue with a changed
  element type is an atomic restart, loudly. State-declared channels
  are always carried when validation passes (undeclared channels die
  with their generation — the "dropped with its generation" rule).
  A preserved channel whose tasks vanished is still carried; blocking
  semantics are the program's concern.
- Queued-value validation cases split (Stage 3 classifies each):
  REACHABLE cases (a `fn`/closure value actually queued in a
  whitelist-valid channel) migrate or restart by rule; DEFENSE-ONLY
  cases (unreachable by construction) are still asserted — the atomic
  commit holds either way.
- Whitelist: channels + plain data only. No timer API exists in v1,
  so none is whitelisted (no timer API is invented to fill it).
  Everything else restarts or refuses loudly. The commit is atomic:
  all-or-validated-carry or full restart.
- Call directionality: retained modules are closed worlds — new code
  may call retained old closures/functions, old code can never call
  new code (old tables reference only old code; preserved `Func`
  values are relinked, never dangling). Production cross-generation
  calls enter through the RETAINED session's tables.
- Callable-valued state: a state binding may hold a `fn` value or a
  closure. A `fn` value is preserved by name-relink against the new
  program's function table and aborts loudly on any name/signature
  mismatch. A closure value is preserved by retaining its module
  while any live holder references it. A callable value that fails
  relink or retention validation aborts the whole reload loudly
  under the atomic-commit rule; it is never silently dropped or
  reinterpreted.

### 14.4 New error codes (fresh `E0316`–`E0326` block)

- `E0316` second `state {}` block in one program.
- `E0317` data state binding without an initializer.
- `E0318` initializer on a channel state binding.
- `E0319` forward (or self) reference in a state initializer.
- `E0320` banned construct in a state initializer (user calls,
  blocking channel ops, task spawn). (`?` needs no E0320 ban: with
  no enclosing function it already fails as E0308, §14.1.)
- `E0321` `migrate` without exactly one parameter.
- `E0322` `migrate` parameter not of struct type `S`.
- `E0323` `migrate` return type not equal to `S`.
- `E0324` duplicate `migrate` for struct `S`.
- `E0325` `migrate` determinism violation (or any direct
  reference/call to a migrate function).
- `E0326` `migrate` not total over `S` (names the first unread field).

## 15. Reserved for later phases (syntax acknowledgment)

- Concurrency: specified in §13 (Phase 3). The `<-` lexical rule (§2.3)
  and the rejection diagnostics (§3.3) were the Phase 1 deliverables.
- FFI: `unsafe extern` blocks, C types (`c_int`, `c_char`, pointers)
  (Phase 5, per D6 rules: rooting for call duration, handle rule, panic
  boundaries, UTF-8 strings, blocking pool, callback queue).
- Hot reload: drain-and-restart only (D7); no syntax in v1 core.
- Optimizations, JIT, GC/scheduler integration: none in Phase 1. The
  interpreter owns a self-contained value model (Rust-owned values with
  reference-counted containers) and must not touch the Spike 1 GC heap,
  the Spike 1b scheduler, or Cranelift.

## 16. Package manager (Phase 7, Stage 3; amendment record 2026-11-17)

No module-model change: flat namespace, path imports, `gzv.std/`
prefix, `pub` accepted with no effect, duplicates `E0311`,
circular imports `E0402` — all unchanged. This section adds
dependency management on top of that model.

### 16.1 Manifest

`toto.toml` keeps `[project]` (name/version/entry) exactly. An
optional `[dependencies]` table maps package names (ascii
alphanumeric/`_`/`-`) to one of:

- `"1.2.3"` — bare version (caret): source-less, always an
  error at resolve time (`E0606`). There is no registry.
- `{ path = "DIR" [, version = "REQ"] }` — directory holding
  `toto.toml` + `.gzv` files (or a `.gzv-pkg` file, unpacked
  into the package cache at resolve time).
- `{ git = "URL", rev = "R" | tag = "T" [, version = "REQ"] }`
  — pinned revision. Branches, HEAD, and unpinned sources are
  rejected (`E0605`).

`REQ` is `^x.y.z` (compatible; bare `x.y.z` means caret),
`~x.y.z` (patch-level), or `=x.y.z` (exact). Versions are
SemVer `MAJOR.MINOR.PATCH` numerics: no prerelease, no build
metadata, and `0.x` is NOT special-cased (ordinary
caret/tilde semantics, already restrictive for `0.x`). No
`[dependencies]` table means exactly today's behavior: no
packages, `toto.lock` ignored when absent.

### 16.2 Resolution (incl. Amendment 1)

First segment rule: `import "<dep>/rest"` loads
`<package-dir>/<rest>` when `<dep>` names a declared
dependency (remainder required; `./` and `../` imports never
match; a bare `import "<dep>"` resolves plainly). `gzv.std/`
is checked first and reserved — no dependency can shadow it.

**Amendment 1 (owner ruling 2026-11-17):** when the literal
import path also resolves from the project root AND the
package file exists, that is an error, loud (`E0601`, naming
the dependency and the root path) — never a silent shadow in
either direction. When only one side exists, it wins (so
adding a dependency never silently rebinds an existing
import). When neither exists, the existing `E0401` applies.
A package dir whose `toto.toml` name differs from the
required name, or a package with the project's own name, is
an error (`E0608`).

Transitive dependencies (Amendment 2a, full resolution): every
selected package's own `[dependencies]` is honored
recursively. One version per package per project
(single-version rule): the highest version satisfying every
requirement (root + transitive, each with provenance);
conflicts name every requirement with its requirer
(`E0602`). Dependency cycles are an error (`E0607`).
Deterministic: same inputs → byte-identical `toto.lock`.

### 16.3 Lockfile

`toto.lock` (checked in) records every selected package
(direct + transitive): name, exact version, source (path |
git URL + pinned rev | pkg file + content hash), and a
content hash over the package's `.gzv` files. It is the ONLY
source of versions at build time. Missing lockfile: resolve
+ write loudly. Lockfile↔manifest disagreement: error
(`E0603`), never a silent re-resolve. Changed content at a
matching selection: error (`E0604`); `toto update` accepts.
Pin behavior (`toto update <pkg>`): packages outside the
updated subtree keep exactly their locked version+source
while satisfiable; an unsatisfiable pin (source moved)
rejoins selection with every requirement still enforced and
every move reported.

### 16.4 Sources and cache; no network at build time

Path sources read live. Git and `.gzv-pkg` sources
materialize into the package cache (`$TOTO_PKG_CACHE`, else
`~/.cache/toto/pkgs`): git clones + checks out the pinned rev
(fetching happens at add/update time only — never at build
time); pkg files unpack by content hash. A locked git rev
missing from the cache fails loudly naming the rev (`E0604`).
Path dependencies do not survive `publish` as paths (machine-
local); republish or re-point them as git/pkg sources.

### 16.5 `add` / `update` / `publish`

- `toto add <name> --path DIR | --git URL (--rev R | --tag T)
  [--version REQ]`, or `toto add <file>.gzv-pkg` (name from
  the packed manifest). Edits `[dependencies]`, resolves,
  writes `toto.lock`. Sourceless names and unpinned git are
  add-time errors.
- `toto update`: re-resolves all requirements, rewrites the
  lock, reports added/changed/removed (or "already up to
  date"). `toto update <pkg>`: pkg + its transitive subtree;
  unknown names are an error (`E0609`).
- `toto publish`: packs `toto.toml` + shipped `.gzv` files
  (no `tests/`, no `target/`, no lockfile) into
  `<name>-<version>.gzv-pkg`, local only. No upload, no
  registry, no auth. `toto add` of a `.gzv-pkg` file treats it
  as a path source (unpacked into the cache).

### 16.6 New error codes (fresh `E0601`–`E0609` block)

- `E0601` ambiguous import: first segment matches a declared
  dependency AND the project-root path (names both).
- `E0602` version conflict (names every requirement + provenance).
- `E0603` lockfile/manifest disagreement.
- `E0604` locked content unavailable or changed (missing
  cached git rev, tampered cache, edited sources).
- `E0605` unpinned git source (no `rev`/`tag`).
- `E0606` source-less requirement.
- `E0607` dependency cycle (names the chain).
- `E0608` cannot load package source (missing manifest, name
  mismatch, git failure, corrupt pkg file, self-named package).
- `E0609` unknown package in `toto update <pkg>`.

Rendered `E0601` attaches to the importing file (existing
diagnostic pipeline); project-level failures (`E0602`–`E0609`)
are CLI errors (exit 3) carrying the code. `E0401`
(unresolvable) and `E0311` (cross-package duplicates) reused
unchanged.

## 17. Networking substrate (Enterprise Wave 1; amendment record 2026-11-19 — DESIGN FOR USER REVIEW)

**[Proposal — the whole section]** No socket API existed (v2.5 open
question). This section designs the smallest API that covers TCP/UDP
sockets + DNS on all supported platforms, with a platform-neutral
shape so deferred Windows can join later without an API redesign.
The user reviews this design at acceptance review; it is flagged
here and in the Wave 1 report, not approved by inattention.

No new builtin type, no new builtin, no D5/type-system change —
everything below is existing types (`int`, `string`,
`Array<int>`, `Result<T, string>`, user structs) plus new
`gzv.std/net` wrappers over new runtime helpers
(`toto_rt_net_*`, recorded in `docs/ffi.md`). If the API as
built needs more than this section, that is a STOP-and-ask,
not a silent extension — and the full bytes / binary-data
story (v2.5 Group C, item 23) stays a separate gated proposal.

### 17.1 Byte representation (the Wave 1 carve-out only)

Socket payloads are `Array<int>` with every element 0–255.
An out-of-range element is a loud `E0308` at send time (the
existing FFI-coercion error — no new code). Received bytes
arrive as `Array<int>`. Rationale: strings cannot hold
arbitrary bytes (UTF-8 invariant), and a real bytestring
type belongs to the Group C bytes story, which this wave
explicitly does not design. The carve-out is exactly: byte
arrays for send/receive, nothing else.

### 17.2 Handles are ints

Sockets are `int` handles (OS fds underneath), opaque like
pointers: never print them in goldens (fd numbers are
process-unstable). Closing twice, or operating on a closed
socket, is a runtime `Err` naming the failure — never a
panic, never silent.

### 17.3 Functions (`gzv.std/net`)

All return `Result<_, string>`; `Err` carries the OS message
(`strerror` discipline — no errno ints cross the boundary).
`Ok` values: fds/counts are `int`; payloads are
`Array<int>`; addresses are `string`s.

- `net_tcp_connect(host: string, port: int) -> Result<int, string>`
- `net_tcp_listen(port: int) -> Result<int, string>` —
  loopback (`127.0.0.1`) only, by decision (§17.4).
- `net_accept(listener: int) -> Result<int, string>`
- `net_send(conn: int, data: Array<int>) -> Result<int, string>` —
  bytes written in this one syscall; may short-send.
- `net_send_all(conn: int, data: Array<int>) -> Result<int, string>` —
  Toto-level loop to full delivery; returns `len(data)`.
- `net_recv(conn: int, max: int) -> Result<Array<int>, string>` —
  up to `max` bytes; an empty array is orderly shutdown,
  NOT an error.
- `net_close(sock: int) -> Result<int, string>` — `Ok(0)`.
- `net_resolve(host: string) -> Result<Array<string>, string>` —
  blocking-pool DNS (genuinely blocking; see §17.5).
- `net_udp_bind(port: int) -> Result<int, string>` — loopback only.
- `net_udp_send_to(sock: int, data: Array<int>, host: string, port: int) -> Result<int, string>`
- `net_udp_recv_from(sock: int, max: int) -> Result<UdpPacket, string>`
  with `pub struct UdpPacket { data: Array<int>, host: string, port: int }`
  declared in `gzv.std/net` (a user-level struct — no type-system change).

Invalid ports (outside 1–65535) and negative `max` are runtime
`Err` strings, not diagnostics (values, not programs, are wrong).

### 17.4 Loopback-only listen/bind (scoped decision)

`net_tcp_listen` and `net_udp_bind` bind `127.0.0.1` only —
no address parameter, no all-interfaces bind. Rationale: the
Wave 1 substrate serves loopback tests and the poller proof;
a public bind address is API surface with security posture
that belongs with the service waves, not the substrate. An
all-interfaces bind is a recorded follow-up, explicitly NOT
raw sockets (Group C stays gated).

### 17.5 Blocking semantics (poller vs pool)

Connect/accept/send/receive waits park the green thread on
the runtime-integrated poller: the fd goes nonblocking, the
task suspends, the worker is free, readiness wakes exactly
that task. Network waits never occupy workers AND never
occupy pool threads (that is what makes it a poller, not a
second pool). DNS resolution parks on the dedicated blocking
pool instead — `getaddrinfo` cannot be made nonblocking, so
it is genuinely blocking per the Phase 3 policy. Boundary
stated as a rule: a wait is poller-eligible iff its fd can
be nonblocking and its readiness is `poll()`able; everything
else (DNS, files) goes to the pool. No timeout parameters
in v1 (timeouts interact with the context-propagation ADR —
designed, not implemented, in this wave); cancellation is
scope cancellation at suspension boundaries only (existing
rule; a parked net wait is a suspension point).

### 17.6 Conformance (what "done" means)

Loopback TCP echo (client + server + concurrent clients),
UDP echo with address round-trip, DNS for `localhost`, every
error path (refused/unresolvable/closed/double-close/short
send), close semantics — green on every supported platform
via the executor (concurrent by nature; AOT execution where
the harness supports it). The poller proof is separate and
stronger (§17.7, wave exit criterion).

### 17.7 Poller correctness proof (wave exit criterion)

No lost wakeups, no worker starvation, demonstrated in the
deterministic concurrency harness (seeded, exact goldens
where deterministic; count-based assertions where the OS
schedules): many clients × many messages, every byte
accounted; a starvation probe showing every parked task
eventually resumes under load. Workloads, seeds/workers,
and run counts recorded in the Wave 1 report — the proof
is evidence, not a timing assertion.

## 18. Cryptography (Enterprise Wave 1; amendment record 2026-11-20)

Primitives are BOUND, never implemented: every digest byte,
tag, ciphertext, signature, random byte, and certificate
verdict comes from the linked system libcrypto (OpenSSL 3 on
Linux, LibreSSL on macOS — the same load-time link model as
libc/libm; see `docs/ffi.md`). No new builtin type, no new
builtin, no D5/type-system change. JIT/AOT refuse the surface
(run with `toto run`/`toto test`); Windows gets check-time
`E0301` like the net surface.

### 18.1 Byte representation

Same carve-out as §17.1: payloads are `Array<int>` with every
element 0–255 (out-of-range is `E0308`). No handles are
retained: every call is one-shot (no contexts, no key objects
crossing the boundary — rooting covers the call).

### 18.2 Functions (`gzv.std/crypto`)

All return `Result<_, string>`; `Err` carries the library
message (fixed strings where the library wording differs
between OpenSSL and LibreSSL, so error goldens hold on every
platform).

- `crypto_sha256(data)` / `crypto_sha512(data)` → digests
  (32/64 bytes).
- `crypto_hmac_sha256(key, data)` /
  `crypto_hmac_sha512(key, data)` → tags (32/64 bytes).
- `crypto_aead_encrypt(key32, nonce12, pt, aad)` → `ciphertext
  || tag(16)` (AES-256-GCM); `crypto_aead_decrypt` authenticates
  or `Err`s.
- `crypto_sign_keygen()` → `SignKeyPair { secret(32),
  public(64) }` (`x || y`, no `0x04` prefix);
  `crypto_sign_sign(secret32, msg)` → DER signature (ECDSA P-256
  over SHA-256); `crypto_sign_verify(public64, msg, sig)` → 1
  (valid), 0 (bad math), `Err` (malformed input).
- `crypto_random(n)` → `n` bytes (`RAND_bytes`, 1–65536).
- `crypto_x509_verify(leaf_pem, ca_pem)` → 1 iff the leaf chains
  to the trust root (0 = bad chain; `Err` = unparseable PEM).
- `crypto_openssl_version()` → the runtime library version
  (infallible; never pinned in goldens).

### 18.3 Conformance (what "done" means)

Known-answer tests against published vectors for every
primitive (NIST SHS digests, RFC 4231 HMAC, pinned AES-GCM and
ECDSA vectors cross-checked with the OpenSSL CLI and Python
`cryptography` before pinning, X.509 fixtures verified with
`openssl verify` at generation), plus error-path negatives —
green on every supported platform via the executor.

## 19. Serialization beyond JSON (Enterprise Wave 1; amendment record 2026-11-20)

Alongside the JSON already in `gzv.std`. Pure Toto code, except
compression (runtime over the mature pure-Rust `flate2`
crate — no C code, identical bytes everywhere). Each format
states its scope; anything outside the scope is a clear error,
never silent reinterpretation. New modules only
(`gzv.std/toml`, `gzv.std/yaml`, `gzv.std/proto`,
`gzv.std/compress`); no new builtin, no type-system change.
Pure-Toto formats work in `toto build` outputs; compression
refuses in JIT/AOT like the other byte-array surfaces.

### 19.1 TOML (`gzv.std/toml`)

Config-shaped subset: tables (incl. dotted `[a.b]` and dotted
keys), string/int/bool/float values, single-line arrays of
scalars, `#` comments. Values are `TomlVal` structs mirroring
the `JsonVal` strata design (nested tables store as source-line
fragments, re-parsed with an absolute-path prefix on access).
NOT covered: inline tables, arrays of tables, datetimes,
multi-line strings, hex/octal/binary ints, `\u` escapes (same
rule as JSON). Empty tables report missing (documented
limitation).

### 19.2 YAML (`gzv.std/yaml`)

Config-shaped subset: block mappings/sequences, nesting by
spaces-only indentation, plain/single/double-quoted scalars,
int/bool/null/float scalars, `#` comments, inline `- k: v`
mappings. Same strata design (nested blocks store dedented,
re-parsed on access); fragments are validated eagerly so
malformed content fails at parse time. NOT covered: anchors,
tags, flow styles, multi-document streams, tabs, `\u`
escapes. Canonical 2-space-indent `yaml_stringify` output is
verified re-parse-stable and cross-checked with PyYAML.

### 19.3 Protobuf (`gzv.std/proto`)

Wire subset: field numbers 1–2²⁹−1, wire types 0 (varint,
non-negative only) and 2 (length-delimited bytes); no
`.proto` compiler, no codegen — schema-driven encode/decode
over explicit field descriptors (`PbField`/`PbSchema`).
Unknown fields are SKIPPED per protobuf rules; unknown wire
types, groups, fixed32/64, and negative ints are clear errors.
Strings cross as bytes (the caller maps text). Conformance:
hand-computed golden wire bytes for a stated contract
(`Person{id=300, name="hi"}`), verified against the encoding
rules independently of the implementation.

### 19.4 Compression (`gzv.std/compress`)

gzip (RFC 1952, default level) via `flate2`/`miniz_oxide`.
Interop is proven against the system utilities, not our own
bytes: the suite decompresses a `gzip -n` oracle vector and
round-trips, and our encoder output is verified with the
system gunzip; compressed bytes are never pinned (headers may
vary). Failures are one fixed string (`decompress: invalid
gzip data`), so error goldens hold everywhere.

## Appendix A. Spec decisions (choices beyond the PDF)

Every **[Proposal]** above is a judgment call the PDF did not fix. For the
reviewer, the load-bearing ones:

1. `fn` keyword; `fn main() -> int` entry returning the exit code; all
   functions require explicit return types; unit type `unit` with literal
   `()` for procedures (review amendment; §3, §3.1).
2. `if` with `else` is an expression; else-less `if`, loops, and assignment
   are statements; blocks evaluate to their trailing expression or `()`
   (§3).
3. Left-to-right evaluation order everywhere, short-circuit `&&`/`||`
   (§5.1).
4. Cross-type `==` panics (except the int/float hint); NaN ≠ NaN; deep
   array/struct equality with cycle fallback to identity (§5.4).
5. Arrays/structs shared by reference; `let` is binding-only immutability;
   shadowing allowed; initializers required (§5.3, §5.6).
6. Closures capture by value at creation; named fns don't capture (§5.7).
7. Strict `bool` conditions; `for` over ranges/arrays only (§5.5).
8. `1.` is not a float; `..` is exclusive-only (§2.4).
9. `len` counts chars; `print` accepts any value; Display rules (§7).
10. Panic/exit-code contract: 0 = main's return, 1 = panic, 2 = static
    diagnostics, 3 = CLI errors; stack-trace format (§6, §10).
11. 8,192-frame recursion limit (§5.9).
12. Float `/` by zero is IEEE (inf/NaN), only int div/mod-by-zero panics.
13. Int/float arithmetic never mixes implicitly; explicit `to_int`/`to_float`.
14. CLI version `0.1.0`.
15. Toto v1 has functions only: no method-call syntax (`x.y(…)` rejected
    with `E0158`); plain field access stays core (review gap-close; §3.3).
16. Statement termination is explicit `;` per §3.2 (confirmed present in
    review; no change needed).
17. Module system (Phase 5): path imports (`import "path";`), flat
    top-level namespace, `pub` accepted with no effect, `mod`/`use` not in
    v1; duplicates across files are `E0311`, circular imports `E0402`,
    unresolved single-file imports `E0401` (§3.1, §3.3, §9.1).
18. Networking substrate (Wave 1): `gzv.std/net` socket API —
    bytes as `Array<int>` 0–255 (the Wave 1 carve-out only; full
    bytes story stays Group C), sockets as opaque `int` handles,
    loopback-only listen/bind, `Result<_, string>` errors, no
    timeouts in v1, poller-vs-pool rule as stated (§17).

Review record (2026-11-14): all items APPROVED — item 1 amended (unit type
added), item 2 amended (else-required rule), items 15–16 added by the
review. All other items approved as proposed. The 25-function builtin
catalog (§7) is FROZEN.

Reopen triggers: any decision reopens on (a) contradiction with a Phase 2
checker requirement, (b) dogfooding evidence it rejects ordinary programs,
or (c) differential-testing friction in Phase 4. Reopening needs a spec
amendment recorded here — never silent drift.

Phase 3 amendment record (2026-11-14): the owner specified the six
concurrency semantics (§13) to resolve the Phase 3 spec-silence blocker:
`chan<T>()` / `chan<T>(n)` construction; `main`-is-root nested scopes;
guarded-block select with random tie-break; `close(ch)` with zero-value
recv plus `closed(ch)`; panic-driven scope unwind; `go` as a unit
statement with no handles. Binding owner decisions. The **[Proposal]**
sub-details in §13 (negative capacity, `E0213`/`E0214`, the zero-value
table, `E0315`, first-panic-wins/settle-then-propagate, deadlock panic,
default-last, `let v = <-ch` binding surface) are flagged for owner
review: confirm or correct, per this appendix's process. The builtin
catalog grows 25 → 27 (`chan`, `close`, `closed`); otherwise frozen.
(Correction 2026-11-17: 27, not 28 — the old total double-counted
`len`; verified against `BUILTIN_NAMES`.)

Phase 5 amendment record (2026-11-16): the owner chose the module model
(path imports over `mod`-trees, flat namespace, meaningless `pub`,
duplicates/circular as static errors). Recorded in §1, §3.1, §3.3, §9.1
and item 17 above.

Stage 2 amendment record (2026-11-16): the `gzv.std/` import prefix
names embedded standard-library modules (`stdlib/*.gzv`, resolved
canonically per §3.1, embedded-first with disk fallback). The stdlib
is ordinary Toto on the frozen builtins; the time module is pure
(durations over int milliseconds, no wall clock) with the clock
deferred to Stage 3 FFI per owner decision. Full API in
`docs/stdlib.md`; execution paths in the Phase 5 report.

Phase 6 amendment record (2026-11-17): declared state and migration
(§14), pinning authorization amendments §2.1–2.7 and §2.15 — the
fn-signature matrix correction (migrate plays no role in signature
changes), no implicit field defaults (any layout change needs a
migrate or the reload restarts loudly), contextual `state`/`migrate`
with a zero-breakage corpus check, one top-level `state {}` block
(`E0316`) with flat-namespace `E0311`, mutable bindings with
declaration-order initializers (forward refs `E0319`, Rust-TDZ
parallel), the `migrate` rules (one param, struct in/out, one per
struct, determinism `E0325`, totality `E0326`), the channel-type row
(empty-queue carries, non-empty changed-type restarts), the
`Heap::is_live` no-under-retention property with a one-collection
over-retention bound, Arc-only retention ownership, new→old-only
call directionality through retained tables, the `toto_*` helper
boundary for generated code, and the reachable/defense-only Q4 split.
Single-generation execution lands with this amendment; reload-time
enforcement is Stage 3.

## Appendix B. Review questions and answers (2026-11-14 review)

All eight questions ANSWERED; answers are binding on Phase 1 implementation.

- Q1. Is `fn main() -> int` (exit code as return) acceptable, or should
  `main` return nothing with a separate exit mechanism?
- Q2. Is panic-on-cross-type-`==` too strict? Should mixed comparisons
  (especially `int == float`) be `false` instead?
- Q3. Should `if` be an expression, or statement-only like loops?
- Q4. Is capture-by-value the right closure rule, or do you want
  capture-by-reference with future checker enforcement?
- Q5. Is the builtin set (§7) too big, too small, or wrongly chosen?
  (Candidates to cut: `floor`/`ceil`, `upper`/`lower`; candidates to add:
  `byte_len`, `assert`.)
- Q6. Should `len` count chars or bytes?
- Q7. Is banning `1.` (trailing-dot float) and `..=` acceptable v1 minimalism?
- Q8. Any objection to the exit-code split (1 panic / 2 static / 3 CLI)?

Answers: A1 APPROVED main-as-exit-code. A2 APPROVED strict `==` with the
int/float hint (Phase 2 rejects cross-type `==` statically, §5.4 note). A3
APPROVED expression-`if` with the else-required rule (§3). A4 APPROVED
capture-by-value with the shared-reference clarification (§5.7). A5 The
25-function catalog stands and is FROZEN — no growth in Phase 1 without
stop-and-ask. A6 `len` counts CHARS. A7 APPROVED minimalism (`1.` is not a
float; no `..=`, no hex floats). A8 APPROVED the exit-code split with the
return-1 collision documented as accepted (§10).
