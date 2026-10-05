# AGENTS.md

Onboarding doc for Claude / agents working on the **Grindlang** crate.

## 1. Project overview

Grindlang is a small, **statically-typed**, **cranelift-JIT-compiled** language that reuses
Lua's surface syntax but is a constrained (Starlark-style) subset, built for one job:
embedding **calculations** and **dialog-tree decisions** into Grindshell.

A Grindlang script is **not** a standalone program. It evaluates to a **module table of
exported functions and constants**; the host compiles it once and calls its exports many
times. Its **top-level code** (top-level statements and constant initializers) runs once, before
the first call, and what it builds is then frozen. State persists between calls only through
**host-provided memory** (Rust-owned).

Two authoritative documents sit beside this one — read them before changing behavior:

- [`SPEC.md`](SPEC.md) — the author-facing language contract: grammar subset, type rules,
  builtins, memory model, the constraint contract, and worked examples. This is the source of
  truth for *what the language means*.
- [`PLAN.md`](PLAN.md) — the phased implementation roadmap and the locked design decisions
  (static inference, f64-only numbers, arena/no-GC, no sandbox). This is the source of truth
  for *why the implementation is shaped the way it is*.

Grindlang is a **separate engine** from dialogmark + Luau, which keep handling Markdown
dialog trees. Grindlang is its own embeddable calc/decision engine and is **not** required to
match Luau's API or semantics.

**Consumers.** `backend/` (`crates/game/src/calc.rs`) and `editor/` (`crates/skill-core`)
path-depend on this crate with default features and compile calc scripts through `api::Engine`:
the shared `grindshell-stats/calc/*.luau`, the backend's own `crates/game/calc/*.luau`, and an
editor project's `calc/*.luau`. A breaking change to `api` or to the Lua surface breaks both builds.

## 2. Status

Phases 0–9 are delivered (front end, resolver, type system, reference interpreter, mid-level
IR, shared runtime, cranelift JIT, host embedding API; plus Phase 9 hardening: differential +
fuzz + property tests, criterion benchmarks with a rough Luau baseline, this `AGENTS.md`, the
`SPEC.md`, the `embed` example, and the `grindlang` CLI runner). Closures with upvalues,
calling first-class function values (see `src/capture.rs`), host-memory method calls
(`mem:m(args)`, `SPEC.md` §7), and top-level code (run once, then frozen — `SPEC.md` §3.1) have
landed since the original Phase 7 cut. Phase 10 added a second surface syntax, a subset of
Ruby's (`SPEC-RUBY.md`, the `ruby` feature), translated into the shared AST. See `PLAN.md` §4
for per-phase notes and what remains deferred (the full native arena, portable closures, and
`SPEC-RUBY.md`'s open items).

## 3. Tech stack

- Edition **2024**. Single crate, internal modules (no workspace).
- `cranelift-*` `0.132` — the JIT backend (`-codegen`, `-frontend`, `-module`, `-jit`,
  `-native`). All cranelift usage is isolated in `src/codegen/` behind the `jit` feature.
- `thiserror` `2` — error enums (matches the sibling repos' convention).
- `serde` `1` + `serde_json` `1` *(optional, behind the `serde` feature)* — marshaling values /
  export signatures across process boundaries, and serializing the IR (`ir::Program`) for the
  CLI runner's on-disk `--cache`. `serde_json` is the on-disk format.
- Dev-only: `criterion` (benchmarks), `proptest` (property/differential tests), `insta`
  (golden snapshots), `mlua` with `luau-jit` (the rough Luau baseline in benchmarks — Luau, not
  stock Lua, matching the rest of Grindshell).

### Cargo features

The crate is **decoupled by backend**. The default gives a fully usable JIT crate with no
other feature required.

- `jit` *(default)* — the cranelift JIT backend (`codegen`, `api`) plus everything it needs.
  Self-contained: enabling only `jit` is enough.
- `interp` — the tree-walking `Interpreter` and the IR `Vm`: the semantics **oracles** and a
  debug execution mode. Independent of `jit`.
- `serde` — serde marshaling of `Value` and export signatures, plus serde derives on the IR
  (`ir::Program` and the `types::Type` / `ast::BinOp`/`UnOp` it references) so a compiled module
  can be cached to / loaded from disk. Pulls in `serde_json` as the on-disk format. The
  `grindlang` CLI **requires** this feature (`required-features = ["jit", "serde"]`) because its
  pyc-style IR cache is default-on; the library itself keeps `serde` optional.
- `ruby` — the Ruby surface (`src/ruby/`, `SPEC-RUBY.md`, `PLAN.md` Phase 10): its lexer and
  parser (`ruby::parse`, to a Ruby-shaped tree) and the translation into the shared
  `ast::Module` (`ruby::parse_module`), which then runs through the Lua pipeline unchanged,
  including the DSL keywords a host declares on `RubyConfig` (`SPEC-RUBY.md` §5). Front end
  only; needs no backend feature. With `jit` it adds `Engine::compile_ruby`, and with `jit` and
  `serde` the CLI runs `.rb` scripts.

**Gating rule:** items needed by *any* execution backend (the runtime `Value` reference
variants `Cell`/`Closure`, `NativeFn`, `ClosureObj`, and the builtin reference impls in
`runtime::builtins`) are gated `#[cfg(any(feature = "interp", feature = "jit"))]` — **not**
on `interp` alone. Only the tree-walking-interpreter-specific pieces (`Value::Function`,
`Value::Native`, `is_truthy`, the `interp` module, `ir::Vm`) are gated on `interp`. Do not
re-introduce a `jit → interp` dependency: `Value` lives in its own always-compiled
`value.rs`, so the JIT does not need the interpreter.

## 4. Commands

```
cargo build                          # default = jit
cargo test                           # jit-only: unit + api + doctests + snapshots
cargo test --features interp         # ALSO runs the interp-vs-jit differential/fuzz/prop suites
cargo test --all-features            # everything (interp + jit + serde + ruby)
cargo bench                          # criterion: parse/compile/call vs Luau; --features interp adds JIT vs interp
cargo run --features serde --bin grindlang -- FILE  # CLI: run FILE's `main` (pyc-style IR cache)
cargo run --features serde,ruby --bin grindlang -- --print-lua FILE.rb  # what a Ruby script becomes
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

> **Important:** the interp-vs-jit **differential, fuzz, and property** suites are gated on
> `all(feature = "interp", feature = "jit")`. Plain `cargo test` (default `jit` only) compiles
> them to **zero tests** — it does *not* exercise the core correctness oracle. Run
> `cargo test --features interp` (or `--all-features`) before trusting a codegen change.

No `mask` / workspace tooling — this crate is freestanding.

## 5. Architecture & pipeline

```
source (.lua-syntax text)
  └─ lexer        tokens (+ spans) + ---@ docs   src/lexer.rs
  └─ parser       AST (Lua-subset grammar)      src/parser.rs, src/ast.rs
       └─ annotations  ---@param/---@return EmmyLua types   src/annotations.rs
source (.rb-syntax text), instead                                     (feature: ruby)
  └─ lexer, parser   Ruby-shaped tree            src/ruby/{lexer,parser,ast}.rs
  └─ translator      the same AST                src/ruby/{scope,desugar,methods,collect,dsl}.rs
(AST → Lua text, to read what a module became)  src/unparse.rs
  └─ resolver     scopes + CONSTRAINT contract  src/resolve.rs
  └─ type checker bidirectional inference       src/types.rs
  └─ lowering     typed mid-level SSA-ish IR     src/ir.rs
       ├─ Interpreter   tree-walking oracle      src/interp.rs        (feature: interp)
       ├─ Vm            IR interpreter oracle     src/ir.rs::vm        (feature: interp)
       └─ codegen       cranelift IR → native     src/codegen/         (feature: jit)
            └─ runtime  reprs, builtins, ops      src/runtime/
  host API: Engine / Module / Value marshaling   src/api.rs           (feature: jit)
  CLI runner: run a file (pyc-style IR cache)       src/bin/grindlang.rs (features: jit + serde)
```

`src/capture.rs` is the shared free-variable / closure-capture analysis (the single source of
truth for upvalue *ordering*) consumed by both `interp` and `ir` lowering so the interpreters
and the JIT agree on a closure's environment.

**Top-level code** (`SPEC.md` §3.1) runs as one more function, `ir::Program::init`, once per
module instance before its first call (every backend has an `init()`; `Engine::compile` calls
it). Constants and top-level `local`s live in **module slots** (`Binding::Module` /
`Binding::TopConst` → `Op::GlobalGet`/`GlobalSet`), never upvalues, so declared functions stay
plain functions. After init each backend freezes everything reachable from the slots
(`Value::collect_reachable`, closure cells included), and its write paths refuse writes into
that set — keep all three doing the same.

Front-end entry points in `lib.rs` are always available: `parse` (→ AST), `check` (+ resolve),
`analyze` (+ typecheck → export signature), `compile` (+ lower → verified IR).
`analyze_module` and `compile_module` (and `Engine::compile_module`) do the same from an
already parsed `ast::Module`, so another surface syntax can feed the shared pipeline;
`Engine::compile_ruby` does it for the Ruby surface. `unparse::to_lua` prints a module from
either surface as Lua (the CLI's `--print-lua`): Lua source prints back as the same tree, and
what a translation holds that Lua can't write prints readably, explained in a comment.

**Surfaces.** `ast::Module::surface` records which syntax a module was parsed from
(`src/surface.rs`: Lua, or Ruby, whose front end is `PLAN.md` Phase 10). It never changes what
a program means. The resolver and checker quote code in diagnostics through it —
`surface.binop(op)`, `surface.nil_test()`, … — so write a new message's code snippets that way
rather than inlining Lua syntax. The Ruby translator makes up nodes Lua source never has, so
look up a binding only for a `Name`, never by the span of another kind of node (see
`src/ruby/desugar.rs`).

### The three-oracle model

Correctness rests on **three independent executors that must agree** on every program:

1. `interp::Interpreter` — tree-walking over the resolved AST (the canonical semantics).
2. `ir::Vm` — a direct interpreter for the lowered IR (tightens the oracle: validates lowering).
3. `codegen::JitModule` — cranelift-compiled native code (the production backend).

Values are compared by their `Display` form (deterministic: sorted table keys,
integer-formatted whole numbers). Any divergence is a bug. This is enforced by
`tests/jit_differential.rs` (curated corpus), `tests/jit_fuzz.rs` (deterministic LCG),
and `tests/prop_differential.rs` (proptest-generated expressions); with the `ruby` feature, the
last two also hold Ruby-surface programs to the same results as their Lua spellings, across
surfaces as well as backends. `tests/common` holds a shared
harness, `Executors`, that builds all of them (plus an eagerly compacting JIT) from source or
from an `ast::Module`; new suites should use it.

### JIT value model (hybrid)

Numbers/bools flow **unboxed** (`f64`/`i8`) so arithmetic, comparisons, branches and loops are
genuinely native. **Reference values flow as `i64` handles** into a per-call runtime context
(`codegen::rt::RtCtx`) that stores `Value`; reference ops (arrays, tables, builtins, host
calls, memory) lower to `extern "C"` calls into `rt_*` shims, delegating heap correctness to
the shared runtime. Errors are recorded in the ctx and surfaced as `Err` after the call. See
the `codegen` and `runtime` module rustdoc for the ABI contract.

## 6. Conventions for contributors / agents

- **SPEC.md is the contract.** A behavior change to the language must be reflected in
  `SPEC.md`. If a question isn't answered there, it's a design decision — resolve and record
  it (and check `PLAN.md`'s locked decisions) before implementing.
- **Two syntaxes, one language.** A language change needs a Ruby spelling — in
  `SPEC-RUBY.md`, in the translator (`src/ruby/`), and in a Ruby test (`tests/ruby_twins/`,
  `tests/ruby_corpus.rs`) — or an explicit "Lua only" in `SPEC-RUBY.md` saying why. A
  diagnostic that quotes code goes through the module's `Surface` (§5), and a new AST node gets
  a spelling in `src/unparse.rs`.
- **Keep the three oracles in agreement.** Any change to semantics, lowering, or codegen must
  keep `Interpreter == Vm == JitModule`. Add/extend cases in the differential, fuzz, or
  property suites; never make a backend pass by special-casing it.
- **Builtins are single-source-of-truth.** Signatures and reference implementations live in
  `runtime::builtins` (the catalog), consumed by the checker, IR lowering, and *all* backends.
  Don't duplicate builtin signature tables. Operator semantics (arithmetic, ordering, `#`,
  array indexing) likewise live once in `runtime::ops`; a backend calls them rather than
  re-implementing them (the JIT's native arithmetic must match `ops::arith`).
- **Declare shims once.** A new `rt_*` shim gets one entry in `codegen::rt`'s `shim_table!`,
  written as its Rust signature; the cranelift declaration is derived from it. Define the shim
  itself inside `define_shims!`, which stops a panic at the shim (one that crosses the
  `extern "C"` boundary ends the process), and guard every call to it in `translate`: any shim
  can fail by panicking.
- **Isolate cranelift.** All cranelift API usage stays in `src/codegen/` behind `jit`, so the
  rest of the crate is insulated from cranelift churn. Pin cranelift versions.
- **Respect the feature gating** (see §3). Shared-runtime items → `any(interp, jit)`;
  interpreter-only items → `interp`. Verify changes with `cargo check --no-default-features`,
  `--no-default-features --features interp`, `--no-default-features --features ruby`,
  `--features serde` (the CLI without the Ruby surface), and default before committing.
- **Diagnostics carry codes + spans.** Every user-facing error is a `Diagnostic` with a stable
  `code` (e.g. `E0001`) and a `Span`; tests match on codes, snapshots on the rendered output.
  Reject unsupported Lua constructs with a clear "not supported in Grindlang" diagnostic, never
  a generic parse error.
- **Trust model: not a sandbox.** Scripts are trusted dev code. No fuel/step budget as a
  security boundary; the optional debug step counter is an authoring aid, off by default.
- **Green bar = `cargo build && cargo test --all-features && cargo clippy --all-targets
  --all-features -- -D warnings`.** Keep it green; the crate is warning-clean. CI
  (`.github/workflows/ci.yml`) runs it on every push to `master` and every pull request, with the
  tests on Linux, Windows and macOS (each has its own native calling convention for the JIT), plus
  `cargo fmt --check` and the feature checks above. It uses the latest stable toolchain, so
  a new Rust release can bring clippy lints the crate doesn't pass yet.
- Edition `2024`. Match the pinned dependency versions unless there's a concrete reason to bump.
