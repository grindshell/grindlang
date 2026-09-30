# Grindlang — Implementation Plan

> Status: Phases 0–8 delivered (front end, resolver, types, interpreter, IR, runtime,
> cranelift JIT, host embedding API); Phase 9 (hardening, tooling, docs) delivered; top-level
> code (run once, then frozen — §2 decision 1) delivered since. See the per-phase "Delivered"
> notes below and [`CLAUDE.md`](CLAUDE.md) for the current shape. Phase 10, a Ruby surface
> ([`SPEC-RUBY.md`](SPEC-RUBY.md), draft), is planned; its first step, R0, is done.

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

1. **Top-level code runs once, then freezes (Starlark-style).** The top level of a script
   is Lua-like: `function name(...)` declarations, `NAME = expr` constants, and ordinary
   statements — `local`s, loops, `if`s, calls — in any order. The statements and constant
   initializers are the module's **top-level code**, which runs **once** per module instance,
   before its first call; everything it builds is then **frozen**, so no call can change what
   the next one sees and host memory stays the only state that persists between calls.
   Scoping follows Lua: a `function` or constant is in scope everywhere, a top-level `local`
   or `local function` only from its declaration on; reading a value before the code that
   sets it has run is a runtime error. `function`s and constants are **the module's exports**
   by default — no `local M = {}` boilerplate, no mandatory `return` — and `local` makes a
   name private. An optional trailing `return { ... }` may re-export/rename a subset for a
   curated public surface. Only top-level code sets module values; a function that tries is
   a compile error. (Superseded the original shape, where the top level was declarations
   only and a constant's initializer had to be a literal expression; chosen over true Luau
   semantics, where top-level locals are mutable module state, to keep §1's rule that only
   host memory persists. See `SPEC.md` §3.1.)
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
   biggest implementation cost — a garbage collector — entirely. One refinement: a loop drops
   what its past iterations created at its back edge, keeping what its variables still hold.
   Without that, a loop building a string by concatenation keeps every intermediate string
   until the call returns, which is quadratic memory. It is still not a GC: nothing is traced,
   because the IR carries a value across a back edge only in a local, so which values survive
   is known from the code (see `codegen::rt`'s module docs). What top-level code builds
   (decision 1) is the exception: it lives as long as the module, in the module's slots, and
   being frozen, it is shared by every call rather than changed by any.
5. **No fuel budget, no sandbox.** Per the trust model, scripts are trusted dev code.
   An *opt-in* debug step counter may exist purely to help developers catch accidental
   infinite loops during authoring, but it is off by default and is not a security or
   resource boundary. The call-depth limit (`SPEC.md` §7.3) and source nesting limit (§8)
   are not budgets either: script calls and every compiler phase run on the host's native
   stack, and overflowing it aborts the process rather than failing the call. They bound
   stack use, not work — an infinite loop still runs forever.
6. **Closures are allowed within an invocation** (they live in the arena) and cannot be
   persisted across calls. Module-level exported functions are compiled once and are not
   closures over per-call state: a module variable they read lives in a module slot, not an
   upvalue. A closure top-level code builds may be kept in a module value, and is frozen with
   it — it can read what it captured, but not change it.
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
- Enforce the **constraint contract**: no free globals (only host-registered names + memory
  handle); banned constructs rejected; determinism rules. (Originally also: no top-level
  locals, top level is the module table + function decls — lifted by top-level code, below.)
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

#### Constants: immutability and per-call identity  ✅ done (memo superseded by top-level code)

A `constdecl` is immutable all the way down (`SPEC.md` §3): writing through one is `E0307`,
resolved in `resolve.rs` by checking what the assignment target is *rooted* in, so `mem.x = …`
stays legal, a local shadowing a constant stays writable, and `C.x = …` does not.

Constants were first **memoized per call** in all three backends — within an invocation every
read yielded the same value, so a constant was equal to itself (reference values compare by
identity, `SPEC.md` §3.5), and the memo died with the call, so nothing survived between calls
(§1). Top-level code (below) replaced the memo: a constant is now built once, when the module
initializes, and frozen, which gives it one identity for the module's lifetime while still
letting nothing change between calls.

Two things surfaced while doing this, both fixed:

- The IR VM's copy of `==` answered `false` for *every* reference pair, so a table was not equal
  to itself — no constants involved. All three backends had their own copy of the rule and one
  had drifted; the differential corpus never compared two reference values, so nothing caught
  it. Equality now lives once, in `Value::ref_eq`.
- `SPEC.md` documented the *typing* of `==` but never its *semantics*, which is what let that
  drift go unnoticed. §3.5 now states value-vs-identity explicitly.

The syntactic check keys on the assignment target's root, so `local t = C; t.x = …` gets past
it. That is closed at run time: a backend records the addresses of every table/array reachable
from a constant (`Value::collect_reachable`), and the write paths refuse an assignment whose
base is in that set. Nested tables and array elements are covered, and reads through an alias
are not affected.

The escape-restriction alternative was rejected: forbidding a composite constant from appearing
anywhere but as the base of a read would have made `local a = C; local b = C; a == b`
unexpressible, i.e. it would have made identity unobservable rather than correct. Freezing keeps
aliasing available for reads, which is what makes identity meaningful.

The one thing a frozen *marker on the value* would buy over this address set is protection for
a constant escaping to the host — the host holds a `Value` and can mutate it through `Rc`
regardless. That is not reachable from script code, and the host is trusted (`PLAN.md` §1
trust model), so it is left alone.

#### Top-level code: run once, then freeze  ✅ done

Evaluated while asking how far Grindlang could move toward Luau: of the three meanings
top-level statements could have in a compile-once, call-many module — a per-call `main`, true
Luau (top-level locals as mutable module state), or Starlark's run-once-then-freeze — the last
keeps §1's rule that only host memory persists, so it is the one built (§2 decision 1, `SPEC.md`
§3.1). Locked sub-decisions: Lua scoping (a `function` or constant is in scope everywhere, a
`local` from its declaration on); top-level code may call host functions but not use memory,
which is bound per instance after it runs; `local` makes a name private; and `NAME = expr`
constants became frozen module values like any other, dropping the literal-only rule (`E0303`)
and the per-call memo.

- **Front end.** The chunk is a list of `TopDecl`s in source order: functions, constants, and
  statements. A top-level `local` is a **module variable** (`Binding::Module`), which every
  function after it reads through a **module slot** — never an upvalue, so declared functions
  stay plain functions (`CallScript`, `E0434`). The resolver enforces the rest: `E0308` (a
  function assigns a module variable), `E0307` extended to module variables (a function writes
  through one), `E0309` (`return` in top-level code), `E0310` (memory in top-level code).
- **IR.** Top-level code lowers into one more function, `Program::init`, which sets slots with
  `Op::GlobalSet`; every function reads them with `Op::GlobalGet`, which fails on a slot not set
  yet (only possible while init runs). `ExportTarget::Global` exports a slot.
- **Backends.** Each runs init once, before the first call (or at `init()`), counting it as no
  script call; a failure is sticky, and a host panic leaves it to run again. Then it freezes
  everything reachable from the slots — including a closure's captured cells, or a closure kept
  in a module value would carry state between calls — and the write paths (field, index, cell)
  refuse a write into that set. Memory is refused while init runs. `Engine::compile` runs init
  eagerly, so its errors are `BuildError::Init`.
- **Cost.** The frozen set is now permanent rather than per call, so any module whose top-level
  code built a composite value pays a hash lookup per table write; one that built none pays the
  one predictable branch as before. In the JIT a `number`/`bool` slot is read with two loads
  from a table of bits filled after init (`RtCtx::global_bits`) rather than through a shim, and
  a reference slot reuses the call's handle for the object when it has one. The
  `call/jit/constread` benchmark (added with this work) measures the constant path; it was 38%
  slower than the per-call memo through the shim alone, and is within about 6% with the fast
  path.

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

> **Delivered.** `src/runtime/` defines the value representation (`repr`: `Repr` —
> `number→f64`, `bool→i8`, references/optionals→`i64` handles) and the
> **single-source-of-truth builtin catalog** (`builtins`) — signatures consumed by the checker
> and IR lowering, reference impls run by both interpreters and the JIT's shims. The previously
> duplicated builtin signature tables were collapsed onto the catalog. Documented in the
> `runtime` module rustdoc.
>
> Phase 6 originally also shipped an offset-based bump `Arena`, `#[repr(C)]` heap layouts, and
> `FnAbi`/`MemorySchema` descriptors, ahead of the backend. Phase 7 took the hybrid handle model
> instead (below), so no backend ever used them, and they were removed rather than maintained as
> a contract nothing honoured. Their design is kept under **Deferred: native arena**.

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
> the first `RunError` in the ctx and return the `ERR` sentinel, and **every fallible op
> branches to a per-function error-exit at its own call site**, so a raised error aborts the
> rest of the call the way it does in both interpreters (`SPEC.md` §7.3); the driver reports
> the error after the call. `JitModule` mirrors the `Vm`/`Interpreter` surface
> (`set_host_function`/`set_memory`/`memory`/`call`). **Validated** by
> `tests/jit_differential.rs` (JIT == AST == IR over the corpus — the third oracle) and
> `tests/jit_fuzz.rs` (a deterministic LCG drives ~2,500 randomized inputs). **Closures with
> upvalues and calling first-class function values have since landed** (`src/capture.rs` is the
> single source of truth for upvalue ordering, shared by both interpreters and the JIT;
> `Module::call_value`/`call_value_typed` invoke a returned closure, which keeps its backing
> code alive via the closure's `origin` stamp), as have **host-memory method calls**
> (`mem:m(args)` lowered to `Op::CallMethod`, `SPEC.md` §7). Still deferred to future work: the
> full native arena (raw-pointer `#[repr(C)]` layouts, below) — the handle runtime is
> incrementally upgradable to it later. The ABI/value-model contract is documented in the
> `codegen` and `runtime` module rustdoc (`SPEC.md` stays the author-facing language contract,
> §1–§9).

#### Deferred: portable (self-contained) closures

A closure is currently **bound to the module instance that built it** (`SPEC.md` §5.5):
`codegen::rt::ClosureOrigin` stamps each closure with its origin module's id, and both
invocation paths — the host `JitModule::call_value` and the in-script `call_indirect` via
`rt_closure_code_addr` — reject a foreign one. That closed a real hole (lifted names like
`make$c0` collide across modules, so name-based resolution silently ran an unrelated function),
but it is the *conservative* resolution: it makes the failure loud rather than making the call
work.

Making closures portable is strictly harder than redirecting dispatch to the origin's code
address, and that shortcut is a trap: a compiled body indexes **its own module's** constant
pools (strings, host-fn ids, memory slots) by baked-in id, so running origin code against the
invoking module's `RtCtx` reads the wrong bindings and can index out of bounds — a panic
unwinding through `extern "C"`. Portability therefore requires carrying the origin's whole
invocation environment on the closure: its `Pools`, its code table, and a *shared* handle to
its host/method/memory registrations (which are mutable per-`JitModule` state today, so they
would have to move behind an `Rc`).

That also raises policy questions worth settling before building it: what does a closure see if
its origin registers a new host function after the closure escaped, and what should happen when
the origin module is dropped but the closure is still invoked — error, or run with no host
bindings? Until a consumer actually needs cross-module closures, the bound-to-origin rule is
the honest contract.

#### Deferred: native arena

The JIT keeps reference values as `Value`s in `RtCtx`'s per-call table and reaches them through
`extern "C"` shims. A native arena would let compiled code load and store them directly. The
design sketched in Phase 6, for whoever picks this up:

- **Allocation.** A bump arena per invocation, reset wholesale when the call returns and keeping
  its capacity, so steady-state calls stop allocating. Nothing is freed individually, so array
  growth reallocates a larger object instead of growing in place. Loops need the same care the
  handle table gets today: a loop resets the arena to its entry mark at its back edge, first
  copying down what its live locals point at, or a concatenating loop's memory is quadratic.
- **Layouts.** Every payload cell is one 8-byte word. A `string` is a `u32` length followed by
  its UTF-8 bytes; an `array` is a `u32` length and capacity followed by its elements (element
  `i` at cell `i - 1`); a `map` is a length and capacity followed by (key handle, value) pairs
  sorted by key, so lookup is a binary search and iteration order matches the interpreters'
  `BTreeMap`. A `record` needs no header or keys: its fields are a flat run of cells in
  sorted-key order, so a field read is a constant-offset load.
- **Host boundary.** A host function's calling convention is its parameter and return `Repr`s;
  a record-typed memory binding becomes one typed accessor per field.

The removed prototype (`runtime::{arena, layout, host}`, `tests/runtime_abi.rs`) is in the git
history before the commit that deleted it.

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
> it is current (keyed by source hash + the executable's build) and otherwise compiles and writes it,
> skipping the front end on an unchanged re-run. `--cache` writes the cache without running (a
> pre-warm step); `--no-cache` runs without touching it. Note this caches the *IR*, not native
> code (cranelift-jit cannot persist compiled code), and still JIT-compiles on load; the IR
> round-trip is covered by `tests/serde_ir.rs`. The differential/fuzz/property suites are
> gated on
> `all(feature = "interp", feature = "jit")`; run `cargo test --features interp`.

### Phase 10 — Ruby surface  (planned)

A second syntax for the same language: a strict subset of Ruby's syntax aimed at game logic and
dialog DSLs. [`SPEC-RUBY.md`](SPEC-RUBY.md) (draft) is its authoring contract; this section is
why it is built the way it is. R0 is done; the rest is not implemented yet.

**Approach: translate into the shared AST.** The backends only see `ir::Program`, and
everything above the IR sees `ast::Module`. A Ruby frontend that translates into `ast::Module`
shares the resolver, the checker, lowering, and all three backends, so every Ruby program is
held to the three-oracle model with no new executor. The alternative — a Ruby AST with its own
resolver and checker, lowering straight to IR — duplicates about 5k lines of front end and
loses the tree-walking `Interpreter`, which walks the AST, as an oracle. The Ruby frontend
therefore produces only what the Lua frontend can, plus the core additions below.

```
src/ruby/        feature `ruby`, off by default; front end only, no backend dependency
  mod.rs         parse_ruby(), RubyConfig, entry points
  lexer.rs       tokens and spans, string interpolation, `# @param` doc lines
  ast.rs         the Ruby-shaped syntax tree
  parser.rs      Ruby subset grammar → ruby::ast
  scope.rs       where each Ruby local is declared
  desugar.rs     ruby::ast → ast::Module
  methods.rs     Ruby method → operator or catalog builtin
  dsl.rs         host-declared keywords → closure tables and records
```

#### Locked decisions (confirmed with stakeholder, 2026-09-30)

| Decision | Choice | Why |
|---|---|---|
| Array indexing | **Ruby indices start at 0.** An index node carries a zero-based flag (C9); lowering and the interpreter add 1 when the base is an array. | The translator can't add the 1 itself: `a[i]` on an array and `m[k]` on a map look alike, and which one it is depends on types. No runtime or backend change. Cost: an out-of-range write reports the 1-based position. |
| String `+` | **Rejected**, with a hint to use interpolation. | Which `+` is meant depends on types, and overloading the core `+` would change the Lua surface. Revisit with a `+` the checker resolves by type if it becomes the top complaint. |
| DSL scope | **Handler and data keywords**, declared by the host. | Both are ordinary top-level code building frozen tables of closures and records, so they need no core change. Anything more waits for scripts that need it. |
| Optional core changes | **C8 in; C7 dropped.** | C7 (`Module::constant`) assumed the host couldn't read a constant's value. It can: `Module::call(name, vec![])` reads one (`SPEC.md` §7.1), so it isn't needed. |

Defaults adopted with them, each specified in SPEC-RUBY.md:

- **Implicit return is syntactic.** An expression in tail position becomes `return e`; an
  assignment, memory write, or loop there returns nothing. The translator needs no types. It
  relies on `return f()` of a call that returns nothing, which now returns nothing (C1), and on
  E0435 rejecting such a call anywhere else.
- **Values from `if`, `case`, and `?:`** use a new conditional expression (C2), not
  `c and a or b`, which yields `b` for a `false` or `nil` `a`. Hoisting into a temporary instead
  would break evaluation order under `&&`, `||`, and loop conditions.
- **Locals are declared where Ruby declares them.** `ruby/scope.rs` emits `local x = …` at a
  first assignment that precedes every use, and otherwise a bare `local x` at the start of the
  scope, which the checker types as optional.
- **The top level maps by capitalization.** `CONST = …` is an exported constant, lowercase is a
  private module variable, `private def` is a plain function left out of a generated export
  table (keeping Ruby's file-wide visibility), and a `def` can't read a top-level lowercase
  local, as in Ruby.
- **`@name` is a memory field** of the binding the host designates; `mem.m(x)` is a memory
  method call.
- **Collection methods with blocks become loops**, accepted only as the whole right side of an
  assignment or a `return`, because builtin signatures (`runtime::builtins::Sig`) have no type
  variables to type a generic `map(arr, fn)`.
- **Calls without parentheses** only at the start of a statement, with restrictions on the first
  argument, which removes Ruby's whitespace-dependent parses without a stateful lexer.
- **Annotations** (`# @param`, `# @return`) reuse `annotations::parse_doc_block` and its type
  syntax. Numbers stay `f64` and conditions `bool`. Ruby-only diagnostics use E05xx (lexing and
  parsing) and E06xx (translation and DSLs).

#### Core changes

The only changes outside `src/ruby/`. Each one that changes semantics updates `SPEC.md` and adds
differential cases (AGENTS.md §6).

- **C1 — `return f()` of a call that returns nothing.** ✅ Landed (7c30866; E0435 in 37fbd61).
  It type-checked but failed the cranelift verifier; it now calls `f` and returns no value.
- **C2 — a conditional expression, `ExprKind::Cond`.** Touches the AST, resolver, capture,
  checker, interpreter, and lowering. The branches join the way `or` operands do (`T` and `nil`
  give `T?`; no covering type is an error) and narrow like an `if`. Lowering reuses the result
  temporary and merge block `lower_logic` builds, so no IR op is added and the Vm and JIT are
  untouched. It has no Lua spelling.
- **C3 — `StatKind::Continue`.** Allowed only inside a loop, like `break`; jumps to the loop's
  step block, or a `while` loop's header. The Lua surface gains Luau's `continue` with it.
- **C4 — `FuncBody::fallthrough`, Error or Nil.** The Lua surface keeps Error (E0480). With
  Nil, reaching `end` in a function that returns a value returns `nil` and makes its return type
  optional, reusing `infer_optional_return`.
- **C5 — entry points that start from an AST.** ✅ Done. `analyze_module` and `compile_module`
  (`analyze` and `compile` now parse and call them), `Engine::compile_module` (likewise under
  `Engine::compile`), and the `Executors` harness in `tests/common`, built from source or from an
  `ast::Module`. The interpreter already took an `ast::Module`.
- **C6 — wording that follows the surface.** ✅ Done, with Lua wording only. The `Surface`
  (`src/surface.rs`) is carried on `ast::Module` rather than in `ResolveConfig` and `TypeConfig`
  as first planned: the parser that built a tree knows its syntax, so the two can't disagree, and
  the configs, which hosts and tests build as struct literals, don't change. The resolver and
  checker quote operators (`surface.binop`/`unop`) and every Lua-specific hint a Ruby script can
  reach (`~= nil`, `ipairs`/`pairs`, the parallel `local`, `cond and a or b`, the wrapping
  closure, the export-table note of `E0309`) through it. Messages the Ruby translator can't
  trigger — the export table's `E0411`, `:` calls' `E0417`, a third `for` variable's `E0416`,
  `ipairs` as a value's `E0301` — keep their Lua wording. Every rewritten message is
  byte-identical to before.
- **C7 — dropped.** See the locked decisions: the host already reads constants.
- **C8 — narrowing on `== nil`.** Narrow `x` in the else-branch of `if x == nil`, including the
  conditions and bodies of later `elseif` arms, and in the else-branch of a C2 conditional.
  Today only the then-branch of `x ~= nil` narrows (`types.rs` `narrowing_from`), so Ruby's
  usual `if x.nil? … else … end` would leave `x` optional. It helps Lua authors equally.
- **C9 — zero-based indexing.** `ExprKind::Index` gains a flag the Lua parser never sets. When
  it is set and the base is an array, lowering and the interpreter add 1 to the index before the
  element read or write; a map or record key is used as written. No IR op, runtime, or backend
  change.

#### Steps

Each step ends on the green bar plus `cargo check --no-default-features --features ruby`. Line
counts exclude tests; the total is about 6k.

- **R0 — seams.** ✅ Done. C5 and C6, with Lua wording only. No behavior change: every existing
  test and snapshot passes untouched.
- **R1 — Ruby lexer and parser** to `ruby::ast`: the grammar of SPEC-RUBY.md §2–3, including
  its rule for calls without parentheses (§3.4), an E05xx rejection for every construct
  SPEC-RUBY.md §6 lists, and the 200-level nesting limit (the E0209 rule). Tests: parse
  fixtures, a snapshot per rejection. (~2.3k lines)
- **R2 — translating the core subset.** C8 and C9 first. Then everything that maps onto
  today's AST: scope analysis, the top level, literals, operators, interpolation, nil tests,
  `if`/`unless`/modifiers, `while`/`until`/`loop`, the iteration forms, memory, the method
  table, lambdas, multiple and compound assignment, annotations, and implicit return of
  expression tails. Exit: the Ruby corpus runs the same on all three oracles, and every twin
  pair lowers to the same IR. (~1.9k lines)
- **R3 — language additions.** C2, C3, and C4, each with its `SPEC.md` section, and the Ruby
  forms that need them: `?:`, `if`/`case` as values, `next`, `nil` on falling off the end.
  (~700 lines)
- **R4 — collection methods** (SPEC-RUBY.md §4.12), with diagnostics for each position a call
  can't be translated in. (~400 lines)
- **R5 — DSL layer.** The keyword declaration API, handler and data keywords, E06xx for unknown
  keywords, repeated ids, and missing fields, and `examples/embed_ruby.rs` with event handlers
  and a small dialog tree. (~700 lines)
- **R6 — integration.** `Engine::compile_ruby`; the CLI's `.rb` detection, `--surface`, the
  surface in the cache key, and an option to print the translated module as Lua for debugging;
  Ruby programs in the fuzz corpus; a parse-and-translate benchmark beside the Lua one; docs
  (SPEC-RUBY.md out of draft, AGENTS.md §5–6, README). (~400 lines)

The first PR is R0: it changes no behavior and turns every later step into a `src/ruby/` change
plus small, separately reviewable core additions.

#### Testing

- **One harness.** Generalize `Executors` from `tests/top_level_code.rs` into `tests/common`,
  so any `ast::Module` runs through the Interpreter, the Vm, and the JIT, which must agree.
- **Twin corpus.** `tests/ruby_twins/` holds `.rb`/`.lua` pairs that must lower to the same
  `ir::Program`, ignoring spans. This tests the translator directly.
- **Snapshots** for every E05xx and E06xx, and for shared checker errors worded for Ruby.
- **Property test.** `prop_differential.rs` prints each generated expression in both syntaxes
  and compares the results across surfaces as well as backends.

#### Risks

- **Ruby habits the core rejects** — every method returning a value, string `+`, integer
  division, truthy objects. Mitigated by SPEC-RUBY.md's "Differences from Ruby" table and a
  Ruby-worded hint on each rejection.
- **Two syntaxes for every future language change.** Each needs a Ruby spelling or an explicit
  "Lua only"; R6 adds that to the AGENTS.md §6 checklist.
- **Iterating the wrong kind of table.** `each` picks `ipairs` or `pairs` by its block's
  parameter count, not by type; a mismatch is E0412, which needs a Ruby-worded hint to be
  understandable.
- **DSL scope creep.** R5 ships two keyword kinds only.
- **Ruby's lexing edge cases.** The call rule and the rejection list keep the grammar small;
  when a form is ambiguous, reject it rather than guess.

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
