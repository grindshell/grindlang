# CLAUDE.md

Onboarding doc for Claude / agents working on the **Grindlang** crate.

## 1. Project overview

Grindlang is a small, **statically-typed**, **cranelift-JIT-compiled** language that reuses
Lua's surface syntax but is a constrained (Starlark-style) subset, built for one job:
embedding **calculations** and **dialog-tree decisions** into Grindshell.

A Grindlang script is **not** a standalone program. It evaluates to a **module table of
exported functions and constants**; the host compiles it once and calls its exports many
times. State persists between calls only through **host-provided memory** (Rust-owned).

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

## 2. Status

Phases 0–9 are delivered (front end, resolver, type system, reference interpreter, mid-level
IR, runtime/ABI, cranelift JIT, host embedding API; plus Phase 9 hardening: differential +
fuzz + property tests, criterion benchmarks with a rough Luau baseline, this `CLAUDE.md`, the
`SPEC.md`, the `embed` example, and the `grindlang` CLI runner). Closures with upvalues and
calling first-class function values have landed since the original Phase 7 cut (see
`src/capture.rs`). See `PLAN.md` §4 for per-phase notes and what remains deferred (method-call
syntax and the full native arena).

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
cargo test --all-features            # everything (interp + jit + serde)
cargo bench                          # criterion: parse/compile/call, JIT vs interp, vs Luau
cargo run --features serde --bin grindlang -- FILE  # CLI: run FILE's `main` (pyc-style IR cache)
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
  └─ lexer        tokens (+ spans)              src/lexer.rs
  └─ parser       AST (Lua-subset grammar)      src/parser.rs, src/ast.rs
  └─ resolver     scopes + CONSTRAINT contract  src/resolve.rs
  └─ type checker bidirectional inference       src/types.rs
  └─ lowering     typed mid-level SSA-ish IR     src/ir.rs
       ├─ Interpreter   tree-walking oracle      src/interp.rs        (feature: interp)
       ├─ Vm            IR interpreter oracle     src/ir.rs::vm        (feature: interp)
       └─ codegen       cranelift IR → native     src/codegen/         (feature: jit)
            └─ runtime  arena, reprs, host ABI, builtins  src/runtime/
  host API: Engine / Module / Value marshaling   src/api.rs           (feature: jit)
  CLI runner: run a file (pyc-style IR cache)       src/bin/grindlang.rs (features: jit + serde)
```

`src/capture.rs` is the shared free-variable / closure-capture analysis (the single source of
truth for upvalue *ordering*) consumed by both `interp` and `ir` lowering so the interpreters
and the JIT agree on a closure's environment.

Front-end entry points in `lib.rs` are always available: `parse` (→ AST), `check` (+ resolve),
`analyze` (+ typecheck → export signature), `compile` (+ lower → verified IR).

### The three-oracle model

Correctness rests on **three independent executors that must agree** on every program:

1. `interp::Interpreter` — tree-walking over the resolved AST (the canonical semantics).
2. `ir::Vm` — a direct interpreter for the lowered IR (tightens the oracle: validates lowering).
3. `codegen::JitModule` — cranelift-compiled native code (the production backend).

Values are compared by their `Display` form (deterministic: sorted table keys,
integer-formatted whole numbers). Any divergence is a bug. This is enforced by
`tests/jit_differential.rs` (curated corpus), `tests/jit_fuzz.rs` (deterministic LCG),
and `tests/prop_differential.rs` (proptest-generated expressions).

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
- **Keep the three oracles in agreement.** Any change to semantics, lowering, or codegen must
  keep `Interpreter == Vm == JitModule`. Add/extend cases in the differential, fuzz, or
  property suites; never make a backend pass by special-casing it.
- **Builtins are single-source-of-truth.** Signatures and reference implementations live in
  `runtime::builtins` (the catalog), consumed by the checker, IR lowering, and *all* backends.
  Don't duplicate builtin signature tables.
- **Isolate cranelift.** All cranelift API usage stays in `src/codegen/` behind `jit`, so the
  rest of the crate is insulated from cranelift churn. Pin cranelift versions.
- **Respect the feature gating** (see §3). Shared-runtime items → `any(interp, jit)`;
  interpreter-only items → `interp`. Verify changes with `cargo check --no-default-features`,
  `--no-default-features --features interp`, and default before committing.
- **Diagnostics carry codes + spans.** Every user-facing error is a `Diagnostic` with a stable
  `code` (e.g. `E0001`) and a `Span`; tests match on codes, snapshots on the rendered output.
  Reject unsupported Lua constructs with a clear "not supported in Grindlang" diagnostic, never
  a generic parse error.
- **Trust model: not a sandbox.** Scripts are trusted dev code. No fuel/step budget as a
  security boundary; the optional debug step counter is an authoring aid, off by default.
- **Green bar = `cargo build && cargo test --all-features && cargo clippy --all-targets
  --all-features -- -D warnings`.** Keep it green; the crate is warning-clean.
- Edition `2024`. Match the pinned dependency versions unless there's a concrete reason to bump.
