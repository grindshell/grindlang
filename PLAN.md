# Grindlang — Implementation Plan

> Status: Phases 0–8 delivered (front end, resolver, types, interpreter, IR, runtime,
> cranelift JIT, host embedding API); Phase 9 (hardening, tooling, docs) delivered. See the
> per-phase "Delivered" notes below and [`CLAUDE.md`](CLAUDE.md) for the current shape.

## 1. What Grindlang is

A small, **statically-typed**, **cranelift-JIT-compiled** language that **reuses Lua's
surface syntax** (so existing Lua syntax highlighters / LSPs work on it) but is a
**constrained subset** — Starlark-style — built for one job: embedding **calculations**
and **dialog-tree decisions** into Grindshell.

It is **not** a standalone program runtime. A Grindlang script behaves like a Lua
module: it evaluates to a **table of exported functions/values**, the host compiles it
once, then calls its exports many times. State persists between calls only through
**host-provided memory** (Rust-owned, first-class interop, like Lua userdata).

### Locked design decisions (confirmed with stakeholder)

| Decision | Choice | Consequence |
|---|---|---|
| Type model | **Static, inferred** | Every expression's type is known at compile time. Native cranelift codegen (f64 / bool / pointers), no tagged-value runtime dispatch. Strict, Starlark-like. |
| Relationship to Luau | **Separate engine, coexists** | dialogmark + Luau keep handling Markdown dialog trees. Grindlang is its own embeddable calc/decision engine; may *later* be offered as an alternate VM, but is **not** required to match Luau's API or semantics. |
| Number model | **f64 only** | One numeric type (classic Lua 5.1 feel). No int/float subtypes, no promotion rules. |

### Syntax mirrors Lua, semantics do not

We accept Lua *syntax* so tooling works, but the language is statically typed and
constrained. Scripts that are valid Lua may be rejected by Grindlang's checker; that is
expected and intended. The README/spec must make this contract explicit so authors
aren't surprised.

### Trust model

Grindlang scripts are written by **developers, not players** — they are trusted code,
not adversarial input. Grindlang is therefore **not a sandbox**: there is no fuel/step
budget enforced as a security boundary, no defensive resource metering, and no
guarding against deliberately hostile scripts. Errors we care about are *developer
mistakes* (type errors, bad memory access), caught at compile time where possible.
This simplifies the runtime considerably.

## 2. Design decisions (resolved)

These follow from the locked decisions in §1 and the trust model above. They are
final design intent, not options — revisit only with a concrete consumer need.

1. **Top-level shape (resolves "no top-level locals").** The top level of a script is a
   set of **export declarations**, not an executable statement list. Two forms:
   `function name(...) ... end` (exported function) and `name = <const-expr>` (exported
   constant, where the RHS is compile-time-evaluable: literals and arithmetic/string
   ops over literals). All top-level names are mutually in scope (so functions can call
   each other and recurse), are **immutable bindings** (not mutable "locals"), and are
   **the module's exports** by default. The implicit module table is assembled from
   them — no `local M = {}` boilerplate, no mandatory `return`. An optional trailing
   `return { ... }` may re-export/rename a subset for a curated public surface. Mutable
   working variables (`local x = ...`) are legal **only inside function bodies**.
2. **`nil` is modeled as optional types `T?`.** Bare `nil` is assignable only to optional
   slots. A `T?` value must be narrowed (via an `if v ~= nil then` check, which flows the
   type to `T` inside the branch) before it can be used as a `T`. The type system stays
   sound; there is no implicit nil-as-anything.
3. **Three static table shapes, inferred from literals/usage:** **records** (fixed string
   keys → per-field types), **arrays** (homogeneous, 1-based), and **maps**
   (`string → T`, homogeneous). No heterogeneous/`any` tables. A table literal's shape is
   inferred; mixing record and array usage on one value is a type error.
4. **Arena-per-invocation, no GC.** Strings/tables created during a call live in a bump
   arena reset wholesale when the call returns. Persistent data goes through host memory
   (Rust-owned). This fits short embedded invocations perfectly and removes the single
   biggest implementation cost — a garbage collector — entirely.
5. **No fuel budget, no sandbox.** Per the trust model, scripts are trusted dev code.
   An *opt-in* debug step counter may exist purely to help developers catch accidental
   infinite loops during authoring, but it is off by default and is not a security or
   resource boundary.
6. **Closures are allowed within an invocation** (they live in the arena) and cannot be
   persisted across calls. Module-level exported functions are compiled once and are not
   closures over per-call state.
7. **No ambient stdlib.** There is no global `os`/`io`/time/random namespace — not for
   sandboxing, but for cleanliness and explicitness. The host injects exactly the
   capabilities a given embedding needs as registered functions; a small curated set of
   pure `math.*`/`string.*` builtins ships in-language.

## 3. Architecture & pipeline

```
source (.lua-syntax text)
  └─ lexer            tokens (+ spans)
  └─ parser           AST (Lua-subset grammar)
  └─ resolver         scopes, name binding, CONSTRAINT ENFORCEMENT
  └─ type checker     bidirectional inference → typed AST + export signature
  ├─ interpreter      reference semantics (oracle + debug fallback)
  └─ lowering         typed mid-level IR (SSA-friendly)
       └─ codegen     cranelift IR → JITModule → native fn pointers
                          └─ runtime (arena, strings, tables, host ABI, builtins)
  host API: Engine / Module / Value marshaling / memory schema
```

### Crate/module layout (single crate, internal modules)

```
src/
  lib.rs            public API re-exports
  diagnostics.rs    spans, error codes, Display; thiserror error enums
  lexer.rs
  ast.rs
  parser.rs
  resolve.rs        scope + constraint enforcement
  types/            type lattice, inference, unification, checking
  ir.rs             mid-level typed IR
  interp.rs         reference interpreter + IR VM oracles (feature: `interp`)
  codegen/          cranelift lowering (feature: `jit`, default)
  runtime/          arena, string/table reprs, host ABI, builtins
  api.rs            Engine, Module, Value, host-fn registration, memory binding
tests/              golden, differential (interp vs jit), integration
benches/            criterion: parse, compile, call; vs interpreter; vs Luau baseline
```

### Dependencies (pin latest stable at implementation time)

- `cranelift-frontend`, `cranelift-codegen`, `cranelift-module`, `cranelift-jit`,
  `cranelift-native`, `target-lexicon` — the JIT backend.
- `thiserror` — error enums (matches ecosystem convention in dialogmark/backend).
- `logos` *(optional)* — fast lexer; or hand-roll to keep deps minimal.
- `serde` / `serde_json` *(optional, behind a feature)* — for marshaling host memory
  and module values across process boundaries, matching the rest of Grindshell.
- `criterion` (dev) — benchmarks. `insta` (dev, optional) — snapshot tests.

## 4. Phased delivery

Each phase is independently testable and leaves the crate green
(`cargo build && cargo test && cargo clippy -- -D warnings`).

### Phase 0 — Scaffolding
- Replace stub `lib.rs`; set up module skeleton, `diagnostics` (spans + error enums),
  and a `Diagnostic`/`Result` convention used everywhere.
- Add deps; gate cranelift behind a `jit` feature and the interpreter behind `interp`
  so the front end can be developed/tested without the backend.
- **Exit:** empty pipeline compiles; error type plumbed end to end.

### Phase 1 — Lexer + parser (front end)
- Lua-compatible lexer: identifiers, keywords, numbers (→ f64), strings incl. long
  brackets `[[ ]]`, comments incl. long comments, operators. Spans on every token.
- Recursive-descent parser for the supported subset → AST. Explicitly **reject**
  unsupported constructs (goto/labels, coroutines, `...` varargs unless chosen, metatable
  syntax, `require`) with a clear "not supported in Grindlang" diagnostic rather than a
  generic parse error.
- **Exit:** parses the example scripts in `tests/fixtures/`; round-trip/AST snapshot tests.

### Phase 2 — Resolver + constraint enforcement
- Scope/name resolution (locals, params, upvalues for in-call closures).
- Enforce the **constraint contract**: no top-level locals; top level is the module
  table + function decls; no free globals (only host-registered names + memory handle);
  banned constructs rejected; determinism rules.
- **Exit:** valid module shapes pass; every constraint violation has a targeted,
  span-pointing error with a test.

### Phase 3 — Type system & inference
- Type lattice: `number` (f64), `bool`, `string`, `nil`/`T?` optionals, `record{…}`,
  `array<T>`, `map<string,T>`, `fn(args)->rets`, host `userdata`.
- Bidirectional checking + local inference; annotations required only at host
  boundaries (registered fn signatures, memory schema). Unify table shapes from usage.
- Extract the **module export signature** (the returned table's field types) and expose
  it to the host so callers know what they can call.
- **Exit:** well-typed programs check; ill-typed programs produce precise errors;
  export signature is queryable.

### Phase 4 — Reference interpreter (semantics oracle)
- Tree-walking interpreter over the typed AST/IR implementing the canonical semantics,
  including arena-style value lifetimes, host-fn calls, and memory access.
- Becomes the **differential-testing oracle** for the JIT and a debug/fallback execution
  mode (`Engine::interpret`).
- **Exit:** end-to-end "compile → call exported fn → get result" works without cranelift;
  drives the public API tests.

### Phase 5 — Mid-level IR + lowering
- Lower typed AST → SSA-friendly typed IR: explicit control-flow graph, typed temporaries,
  desugared loops/conditionals, explicit table/string/builtin/host-call ops. (Optional
  debug step-counter hooks at back-edges, off by default — see §2.5.)
- Keep IR backend-agnostic (interpreter can optionally run it too, tightening the oracle).
- **Exit:** IR builder + verifier; IR-level tests.

### Phase 6 — Runtime & memory model  ✅ done (`src/runtime/`, ABI-first)
- **Arena allocator** (bump, reset per invocation). String repr (immutable bytes in
  arena; consider interning for constants). Record/array/map memory layouts.
- **Host ABI:** calling convention for registered Rust functions; the **memory userdata
  ABI** (typed accessors that compile to direct Rust calls into host-owned state).
- **Builtins:** a curated subset of `math.*` and `string.*` implemented as runtime calls
  (no global stdlib namespace pollution; deterministic only).
- **Exit:** runtime callable from the interpreter; ABI documented and tested.

> **Delivered (ABI-first; raw-pointer/`unsafe` arena addresses deferred to Phase 7).**
> `src/runtime/` defines the value representation (`repr`: `Slot`/`Repr` — `number→f64`,
> `bool→i8`, references/optionals→`i64` arena handles), a reset-per-invocation bump `Arena`
> (offset-based, capacity-retaining), `#[repr(C)]` heap layouts for string/array/map/record
> (`layout`), the host calling convention `FnAbi` + memory userdata `MemorySchema` (`host`),
> and the **single-source-of-truth builtin catalog** (`builtins`) — signatures consumed by
> the checker and IR lowering, reference impls run by both interpreters. The previously
> duplicated builtin signature tables were collapsed onto the catalog. Tested via the runtime
> unit tests, `tests/runtime_abi.rs`, and the existing interpreter/differential suites (which
> now dispatch builtins through the runtime). Documented in the `runtime` module rustdoc.

### Phase 7 — Cranelift codegen / JIT  ✅ done (`src/codegen/`, `jit` feature)
- Map IR → cranelift IR per function. Value reprs: `f64` for number, `i8` for bool,
  `i64` pointers for string/table/userdata. Wire calling convention, host-fn trampolines,
  memory accessors, builtins, and trap → `Err` translation.
- `JITModule` build/finalize → native function pointers held by `Module`.
- **Exit:** JIT path passes the **same** test suite as the interpreter via differential
  testing (interp result == jit result for a large corpus, incl. fuzzed inputs).

> **Delivered (hybrid value model — stakeholder decision).** `src/codegen/` (gated `jit`,
> cranelift 0.132) compiles the typed IR to native code. Numbers/bools flow **unboxed**
> (`f64`/`i8`) so arithmetic, comparisons, branches, loops, and numeric-for are genuinely
> native cranelift; **reference values flow as `i64` handles** into a per-call runtime
> context (`codegen::rt::RtCtx`) that stores `interp::Value`, and reference ops
> (MakeArray/FieldGet/MapGet/builtins/host/memory) lower to `extern "C"` calls into `rt_*`
> shims — heap correctness delegated to the proven runtime. The ctx is a hidden first param;
> each export gets a uniform `(ctx, argv, argc) -> handle` trampoline. Errors: shims record
> the first `RunError` in the ctx and return a null/`ERR` sentinel; loop back-edges check
> `rt_errored` and divert to a per-function error-exit; the driver reports `ctx.error` after
> the call. `JitModule` mirrors the `Vm`/`Interpreter` surface
> (`set_host_function`/`set_memory`/`memory`/`call`). **Validated** by
> `tests/jit_differential.rs` (JIT == AST == IR over the corpus — the third oracle) and
> `tests/jit_fuzz.rs` (a deterministic LCG drives ~2,500 randomized inputs). **Closures with
> upvalues and calling first-class function values have since landed** (`src/capture.rs` is the
> single source of truth for upvalue ordering, shared by both interpreters and the JIT;
> `Module::call_value`/`call_value_typed` invoke a returned closure, which keeps its backing
> code alive via the closure's `keepalive`). Still deferred to future work: method-call syntax
> and the full native arena (raw-pointer `#[repr(C)]` layouts) — the handle runtime is
> incrementally upgradable to it later. The ABI/value-model contract is documented in the
> `codegen` and `runtime` module rustdoc (`SPEC.md` stays the author-facing language contract,
> §1–§9).

### Phase 8 — Host embedding API
- `Engine` (owns cranelift/JIT context, builtin registry), `Module` (compiled script +
  export signature + fn pointers), `Value` (typed Rust↔Grindlang marshaling).
- Register host functions; declare the **memory schema** (Rust type ↔ Grindlang
  userdata); call exports by name with typed args; surface errors as `thiserror` enums.
- Module caching / recompile-on-change; optional `serde` marshaling behind a feature.
- **Exit:** a clean, documented embedding API with examples; the API is the tested surface.

### Phase 9 — Hardening, tooling, docs  ✅ done
- Differential + property + fuzz tests (parser, checker, interp-vs-jit). Golden snapshots.
- Benchmarks: parse / compile / call latency; JIT vs interpreter; rough vs Luau baseline
  for a representative calc and a dialog-decision script.
- Write `CLAUDE.md` (conventions, like sibling repos) and a language `SPEC.md`
  (grammar subset, type rules, builtins, memory model, constraints).
- A worked **integration spike**: a standalone example embedding Grindlang to make a
  dialog-tree decision and run a stat calculation against host memory — proving the
  coexist-with-Luau story without touching the other repos.

> **Delivered.** Testing now spans the curated differential corpus
> (`tests/jit_differential.rs`), the deterministic LCG fuzzer (`tests/jit_fuzz.rs`), a
> **proptest** property suite that generates random expression programs *and* inputs and
> shrinks any AST==IR==JIT divergence to a minimal case (`tests/prop_differential.rs`), and
> **insta** golden snapshots pinning rendered diagnostics and inferred export signatures
> (`tests/snapshots.rs`, `tests/snapshots/`). `benches/grindlang.rs` (criterion) measures
> front-end parse/analyze/compile latency, backend compile (grindlang JIT vs. Luau), and call
> latency across all three grindlang executors plus a **Luau baseline** (`mlua`, `luau-jit`)
> over `fib`/`loopsum`/`mitigated` workloads — on a sample run the cranelift JIT calls a
> recursive `fib` ~7× faster than Luau and orders of magnitude faster than the tree-walking
> interpreter. Docs: this plan, [`CLAUDE.md`](CLAUDE.md) (agent onboarding, mirroring the
> sibling repos), and [`SPEC.md`](SPEC.md) (the author-facing language contract). The
> integration spike ships as `examples/embed.rs` (a stat calc + a dialog decision against host
> memory, run with `cargo run --example embed`). A small **CLI runner** ships as the
> `grindlang` binary (`src/bin/grindlang.rs`, `required-features = ["jit", "serde"]`): it
> compiles a script file and invokes a chosen entry export (`main` by default, or `--call NAME`)
> with no arguments, printing the returned value — a quick way to exercise a module without
> writing an embedding harness. It caches the lowered IR (`ir::Program`, serde-derived behind the
> `serde` feature; `serde_json` on disk) **pyc-style by default**: a run reads `<FILE>.glir` when
> it is current (keyed by source hash + binary version) and otherwise compiles and writes it,
> skipping the front end on an unchanged re-run. `--cache` writes the cache without running (a
> pre-warm step); `--no-cache` runs without touching it. Note this caches the *IR*, not native
> code (cranelift-jit cannot persist compiled code), and still JIT-compiles on load; the IR
> round-trip is covered by `tests/serde_ir.rs`. The differential/fuzz/property suites are
> gated on
> `all(feature = "interp", feature = "jit")`; run `cargo test --features interp`.

## 5. Risks & mitigations

- **JIT-of-a-language is the hard part.** Mitigated by: static types (no dynamic
  dispatch), f64-only numbers, arena/no-GC, and an interpreter-first oracle so the JIT is
  validated by differential testing rather than written blind.
- **Type system scope creep.** Keep the lattice small (no generics beyond
  array/map element types, no user-defined types initially). Revisit only if a real
  consumer needs more.
- **cranelift API churn.** Pin versions; isolate all cranelift usage in `codegen/` behind
  the `jit` feature so the rest of the crate is insulated.

## 6. Suggested first PR

Phases 0–1 together: scaffolding, diagnostics, lexer, parser, and a `tests/fixtures/`
corpus of example Grindlang modules (a stat calc, a dialog decision). That establishes the
syntax contract and the test harness everything else builds on, with zero cranelift risk.
