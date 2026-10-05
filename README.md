# Grindlang

Grindlang is a statically-typed, Cranelift-JIT-compiled embedded language for
calculations and dialog-tree decisions in [Grindshell](../). It reuses Lua's surface
syntax but is a constrained, Starlark-style subset: a script that is valid Lua may be
rejected by Grindlang's checker, by design.

A Grindlang script is not a standalone program. It evaluates to a **module** — a table of
exported functions and constants. The host compiles a script once and calls its exports
many times. The only state that persists between calls is host-provided memory, owned by
Rust.

A script can also be written in a second syntax, a strict subset of Ruby's, for game logic and
dialog DSLs (the `ruby` feature; see [Ruby surface](#ruby-surface)). It means exactly what the
same script in Lua syntax would.

## Design

- **Statically typed, mostly inferred.** Every expression has a type known at compile
  time. Parameter and return types are inferred bidirectionally from use; optional
  EmmyLua `---@` annotations pin types where inference is insufficient.
- **One numeric type.** All numbers are `f64`. No integer subtype and no bitwise
  operations.
- **Top-level code runs once.** Constants and top-level statements run once, when the
  module initializes, and what they build is then frozen. There are no free globals, no
  implicit `_G`, and no ambient standard library.
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

## Ruby surface

With the `ruby` feature, the same module can be written in a strict subset of Ruby's syntax,
specified in [`SPEC-RUBY.md`](SPEC-RUBY.md). The translator turns it into the same tree the Lua
parser builds, so the checker, the IR, and all three executors are shared:

```ruby
ARMOR_K = 100

# @param attack number
# @param armor number
# @return number
def mitigated(attack, armor)
  attack * (ARMOR_K / (ARMOR_K + armor))
end

def lethal(attack, armor, hp)
  mitigated(attack, armor) >= hp
end
```

Ruby's habits that Grindlang's types rule out — truthy objects, `"a" + "b"`, integer division —
are rejected with a hint saying what to write instead, and indices start at 0, as in Ruby.

The host can declare **DSL keywords**: a handler keyword collects blocks into a table of
closures, and a data keyword builds records, such as a dialog tree's nodes and choices.
`@name` reads a field of the host memory the host designates:

```rust
use grindlang::ruby::RubyConfig;
use grindlang::{Type, Value};

let mut ruby = RubyConfig::new().ivar_memory("mem");
ruby.handler("on", "ON")
    .block(&[Type::Number], Type::Unit)
    .dispatch("on_event");

// on :hit do |dmg|
//   @hp -= dmg
// end
let mut module = engine.compile_ruby(src, &ruby).expect("compile module");
module.call("on_event", vec![Value::string("hit".to_string()), Value::Number(7.0)])?;
```

[`examples/embed_ruby.rs`](examples/embed_ruby.rs) handles events and walks a dialog tree:

```
cargo run --example embed_ruby --features ruby
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

A `.rb` file is read as the Ruby surface, which needs the `ruby` feature too; `--surface lua`
or `--surface ruby` names the syntax whatever the extension. `--print-lua` prints what a
script becomes, as Lua, without checking or running it — for a Ruby script, its translation:

```
cargo run --features serde,ruby --bin grindlang -- script.rb               # run script.rb's `main`
cargo run --features serde,ruby --bin grindlang -- --print-lua script.rb   # show its translation
```

### Disk cache (pyc-style)

The Cranelift JIT compiles into process memory and cannot be persisted, so there is no
native-code cache. What the runner caches is the **lowered IR** (`ir::Program`), and — like
Python's `*.pyc` — it does so **by default**: a normal run reads `<FILE>.glir` when it is present
and current (deserializing the IR and skipping the front end, still JIT-compiling on load), and
otherwise compiles the source and writes the cache. It is keyed by a source hash, the syntax
the source was read in, and the identity of the `grindlang` executable that wrote it (version,
size, and modification time), so editing the script or rebuilding `grindlang` invalidates it
automatically.

Three flags adjust this:

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
source (.rb-syntax text), instead                                (feature: ruby)
  └─ lexer, parser, translator → the same AST   src/ruby/
  └─ resolver     scopes + constraint contract  src/resolve.rs
  └─ type checker bidirectional inference       src/types.rs
  └─ lowering     typed mid-level IR            src/ir.rs
       ├─ Interpreter   tree-walking oracle      src/interp.rs   (feature: interp)
       ├─ Vm            IR interpreter oracle     src/ir.rs::vm   (feature: interp)
       └─ codegen       Cranelift IR → native     src/codegen/    (feature: jit)
            └─ runtime  reprs, builtins, ops      src/runtime/
  host API: Engine / Module / Value marshaling   src/api.rs      (feature: jit)
```

Front-end entry points are always available regardless of backend: `parse`, `check`,
`analyze`, and `compile`, and `analyze_module` and `compile_module` for a module parsed from
either syntax. `unparse::to_lua` prints a module as Lua.

### Three-oracle correctness model

Correctness rests on three independent executors that must agree on every program:

1. `interp::Interpreter` — tree-walking over the resolved AST; the canonical semantics.
2. `ir::Vm` — a direct interpreter for the lowered IR; validates lowering.
3. `codegen::JitModule` — Cranelift-compiled native code; the production backend.

Values are compared by their deterministic `Display` form (sorted table keys,
integer-formatted whole numbers). Any divergence is a bug, enforced by the differential
(`tests/jit_differential.rs`), fuzz (`tests/jit_fuzz.rs`), and property
(`tests/prop_differential.rs`) suites. The fuzz and property suites also hold Ruby-surface
programs to the results of their Lua spellings.

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
| `ruby` | no | The Ruby surface: its parser and translator, and the DSL keywords. Front end only; with `jit` it adds `Engine::compile_ruby`, and the CLI reads `.rb` scripts. |

## Commands

```
cargo build                          # default = jit
cargo test                           # jit-only: unit + api + doctests + snapshots
cargo test --features interp         # ALSO runs the interp-vs-jit differential/fuzz/property suites
cargo test --all-features            # everything (interp + jit + serde + ruby)
cargo bench                          # criterion: parse/compile/call vs a Luau baseline
cargo bench --features interp        # ALSO JIT vs the two interpreters
cargo bench --features ruby          # ALSO the Ruby surface's front end, beside the Lua one's
cargo run --features serde --bin grindlang -- FILE  # CLI runner (caches IR; needs serde)
cargo run --features serde,ruby --bin grindlang -- FILE.rb  # a Ruby-surface script
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

The interp-vs-jit differential, fuzz, and property suites are gated on
`all(feature = "interp", feature = "jit")`. Plain `cargo test` (default `jit` only) compiles
them to zero tests and does not exercise the correctness oracle — run
`cargo test --features interp` (or `--all-features`) before trusting a codegen change.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs `cargo test --all-features` on
Linux, Windows and macOS, plus rustfmt, clippy and a build of each feature combination, on every
push to `master` and every pull request.

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
mid-level IR, shared runtime, Cranelift JIT, the host embedding API, and Phase 9 hardening
(differential/fuzz/property tests, benchmarks, docs, the embedding example, and the CLI
runner). Closures with upvalues and calling first-class function values are supported.
Host-memory method calls (`mem:m(args)`) are supported; the full native arena remains deferred.
Phase 10 added the Ruby surface, with host-declared DSL keywords; its open items are listed in
[`SPEC-RUBY.md`](SPEC-RUBY.md). See [`PLAN.md`](PLAN.md) for per-phase exit criteria and
deferred work.

## License

Licensed under either of MIT or Apache-2.0 at your option.
