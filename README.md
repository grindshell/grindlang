# Grindlang

Grindlang is a statically-typed, Cranelift-JIT-compiled embedded language for
calculations and dialog-tree decisions in [Grindshell](../). It reuses Lua's surface
syntax but is a constrained, Starlark-style subset: a script that is valid Lua may be
rejected by Grindlang's checker, by design.

A Grindlang script is not a standalone program. It evaluates to a **module** — a table of
exported functions and constants. The host compiles a script once and calls its exports
many times. The only state that persists between calls is host-provided memory, owned by
Rust.

## Design

- **Statically typed, mostly inferred.** Every expression has a type known at compile
  time. Parameter and return types are inferred bidirectionally from use; optional
  EmmyLua `---@` annotations pin types where inference is insufficient.
- **One numeric type.** All numbers are `f64`. No integer subtype and no bitwise
  operations.
- **Constrained top level.** The top level contains only export declarations — no
  top-level locals, no free globals, no implicit `_G`, no ambient standard library, and no
  executable statements at load time.
- **Conditions must be `bool`.** Unlike Lua, there is no truthiness coercion; narrow an
  optional with an explicit comparison (`if v ~= nil then …`).
- **Host-injected capabilities.** Randomness, game queries, and persistent state are not
  builtins. The host registers functions and declares a memory schema; the script reaches
  the outside world only through those.
- **Not a sandbox.** Scripts are trusted developer code. There is no fuel or step budget
  as a security boundary.

Grindlang is a separate engine from dialogmark + Luau, which continue to handle Markdown
dialog trees. It is not required to match Luau's API or semantics.

See [`SPEC.md`](SPEC.md) for the full language contract (grammar, type rules, builtins,
memory model, worked examples) and [`PLAN.md`](PLAN.md) for the phased implementation
roadmap and locked design decisions.

## Example

A stat-calculation module:

```lua
ARMOR_K = 100

---@param attack number
---@param armor number
---@return number
function mitigated(attack, armor)
  return attack * (ARMOR_K / (ARMOR_K + armor))
end

function lethal(attack, armor, hp)
  return mitigated(attack, armor) >= hp
end
```

This exports `{ ARMOR_K, mitigated, lethal }` with their inferred types. To curate the
public surface, end the script with a `return` table that maps exported names to keys.

Embedding from Rust:

```rust
use grindlang::api::Engine;

let mut engine = Engine::new();
engine.register_fn("difficulty", || 1.25_f64);

let mut module = engine.compile(src).expect("compile module");
let dmg: f64 = module.call_typed("mitigated", (120.0, 80.0)).unwrap();
```

A runnable version, including a dialog-tree decision driven against host memory, is in
[`examples/embed.rs`](examples/embed.rs):

```
cargo run --example embed --features jit
```

## Command-line runner

The `grindlang` binary compiles a script file and invokes one of its exports — `main` by
default, or another with `--call NAME` — calling it with no arguments and printing the
returned value. Because it binds no host functions or memory, a script run this way may use
only the in-language builtins; anything needing host capabilities goes through the embedding
API above. The runner requires the `serde` feature (its disk cache, below, depends on it), so
build/run it with `--features serde`:

```
cargo run --features serde --bin grindlang -- script.lua             # run script.lua's `main`
cargo run --features serde --bin grindlang -- --call total sums.lua  # run the `total` export
```

Given a script whose `main` returns `fib(10)`, it prints `55`. A missing or unsuitable entry
export (absent, parameterized, or not a function) is reported to stderr with a non-zero exit.

### Disk cache (pyc-style)

The Cranelift JIT compiles into process memory and cannot be persisted, so there is no
native-code cache. What the runner caches is the **lowered IR** (`ir::Program`), and — like
Python's `*.pyc` — it does so **by default**: a normal run reads `<FILE>.glir` when it is present
and current (deserializing the IR and skipping the front end, still JIT-compiling on load), and
otherwise compiles the source and writes the cache. It is keyed by a source hash plus the
binary's version, so editing the script or rebuilding `grindlang` invalidates it automatically.

Two flags adjust this:

- `--cache` compiles and writes the IR cache **without running** the script (a pre-warm step).
- `--no-cache` runs the script but neither reads nor writes the cache.
- `--cache-file P` uses path `P` instead of `<FILE>.glir`.

```
cargo run --features serde --bin grindlang -- script.lua            # run; reuse or write cache
cargo run --features serde --bin grindlang -- --cache script.lua    # write cache, do not run
cargo run --features serde --bin grindlang -- --no-cache script.lua # run, ignore the cache
```

## Architecture

```
source (.lua-syntax text)
  └─ lexer        tokens (+ spans)              src/lexer.rs
  └─ parser       AST (Lua-subset grammar)      src/parser.rs, src/ast.rs
  └─ resolver     scopes + constraint contract  src/resolve.rs
  └─ type checker bidirectional inference       src/types.rs
  └─ lowering     typed mid-level IR            src/ir.rs
       ├─ Interpreter   tree-walking oracle      src/interp.rs   (feature: interp)
       ├─ Vm            IR interpreter oracle     src/ir.rs::vm   (feature: interp)
       └─ codegen       Cranelift IR → native     src/codegen/    (feature: jit)
            └─ runtime  arena, reprs, host ABI    src/runtime/
  host API: Engine / Module / Value marshaling   src/api.rs      (feature: jit)
```

Front-end entry points are always available regardless of backend: `parse`, `check`,
`analyze`, and `compile`.

### Three-oracle correctness model

Correctness rests on three independent executors that must agree on every program:

1. `interp::Interpreter` — tree-walking over the resolved AST; the canonical semantics.
2. `ir::Vm` — a direct interpreter for the lowered IR; validates lowering.
3. `codegen::JitModule` — Cranelift-compiled native code; the production backend.

Values are compared by their deterministic `Display` form (sorted table keys,
integer-formatted whole numbers). Any divergence is a bug, enforced by the differential
(`tests/jit_differential.rs`), fuzz (`tests/jit_fuzz.rs`), and property
(`tests/prop_differential.rs`) suites.

### JIT value model

Numbers and bools flow unboxed (`f64` / `i8`), so arithmetic, comparisons, branches, and
loops are native. Reference values flow as `i64` handles into a per-call runtime context
that stores the `Value`; reference operations (arrays, tables, builtins, host calls,
memory) lower to `extern "C"` calls into `rt_*` shims that delegate heap correctness to the
shared runtime.

## Features

The crate is decoupled by backend. The default gives a fully usable JIT crate.

| Feature | Default | Description |
|---|---|---|
| `jit` | yes | Cranelift JIT backend — the production execution path. Self-contained. |
| `interp` | no | Tree-walking interpreter and IR VM — the semantics oracles and a debug execution mode. Independent of `jit`. |
| `serde` | no | Serde marshaling of `Value` and export signatures, plus serde derives on the IR (`ir::Program`) so a compiled module can be cached to disk. Required by the `grindlang` CLI runner (its pyc-style IR cache is default-on). |

## Commands

```
cargo build                          # default = jit
cargo test                           # jit-only: unit + api + doctests + snapshots
cargo test --features interp         # ALSO runs the interp-vs-jit differential/fuzz/property suites
cargo test --all-features            # everything (interp + jit + serde)
cargo bench                          # criterion: parse/compile/call, JIT vs interp, vs a Luau baseline
cargo run --features serde --bin grindlang -- FILE  # CLI runner (caches IR; needs serde)
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

The interp-vs-jit differential, fuzz, and property suites are gated on
`all(feature = "interp", feature = "jit")`. Plain `cargo test` (default `jit` only) compiles
them to zero tests and does not exercise the correctness oracle — run
`cargo test --features interp` (or `--all-features`) before trusting a codegen change.

## Benchmarks vs Luau

The benchmark suite ([`benches/grindlang.rs`](benches/grindlang.rs)) times three workloads
against a Luau baseline (`mlua` with `luau-jit`, matching the rest of Grindshell). Each
Luau program is the identical computation, so the two are comparable. The workloads are
`fib(20)` (recursion-heavy), `loopsum(1000)` (a tight arithmetic loop), and
`mitigated(120, 80)` (a small branching calculation — the representative embedding case).

Numbers below are criterion medians from one run on an AMD Ryzen 7 5800X (Windows, release
build, `cargo bench --features interp`). Treat them as indicative, not a controlled
cross-language shootout: absolute values vary with hardware and load, and the Luau side is
a rough baseline. Reproduce with `cargo bench`.

### Call latency (steady-state execution)

| Workload | Grindlang JIT | Luau | Ratio |
|---|---|---|---|
| `fib(20)` | 37.0 µs | 253.9 µs | 6.9× faster |
| `loopsum(1000)` | 2.42 µs | 2.86 µs | 1.2× faster |
| `mitigated(120, 80)` | 119.1 ns | 126.0 ns | 1.06× faster |

The Cranelift-compiled native code wins decisively on the call-dominated recursive
workload and is roughly even on the arithmetic loop. On the very short branching
calculation — where the call boundary dominates and a few nanoseconds of overhead swing
the result — the two are within ~6%, with Grindlang now marginally ahead.

### Backend compile time (one-time, per module)

| Workload | Grindlang JIT | Luau |
|---|---|---|
| `fib` | 175.0 µs | 67.5 µs |
| `loopsum` | 185.8 µs | 39.7 µs |
| `mitigated` | 158.1 µs | 60.6 µs |

Grindlang's Cranelift backend spends more producing native code than Luau's bytecode
compiler does. This is the intended trade-off: a module is compiled once and its exports
are called many times, so compile cost is amortized while call latency is what the host
pays repeatedly.

## Status

Phases 0–9 are delivered: front end, resolver, type system, reference interpreter,
mid-level IR, runtime/ABI, Cranelift JIT, the host embedding API, and Phase 9 hardening
(differential/fuzz/property tests, benchmarks, docs, the embedding example, and the CLI
runner). Closures with upvalues and calling first-class function values are supported.
Method-call syntax and the full native arena remain deferred. See [`PLAN.md`](PLAN.md) for
per-phase exit criteria and deferred work.

## License

Licensed under either of MIT or Apache-2.0 at your option.
