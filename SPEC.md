# Grindlang — Language Specification (draft)

> Companion to [PLAN.md](PLAN.md). This document defines the **lexical structure**,
> **grammar**, **module model**, and the **type/interop rules** needed to read the
> grammar. It is the authoring contract for `.lua`-syntax Grindlang scripts.
>
> Status: draft. Sections marked _(v1)_ are the initial target; _(later)_ items are
> deliberately deferred.

## 1. What a Grindlang program is

A Grindlang script is **not** a standalone program. It is a **module definition**: it
evaluates to a **table of exported functions and constants**. The host compiles a script once
into a `Module`, then calls its exported functions many times. The script's **top-level
code** — its top-level statements and constant initializers — runs **once**, before the first
call, and everything it builds is then **frozen** (§3.1). The only state that survives between
calls is **host-provided memory** (Rust-owned, exposed as a userdata-like value — see §7).

Grindlang reuses **Lua's surface syntax** so existing Lua syntax highlighters and LSPs
work on it, but it is **statically typed** and a **constrained subset** of Lua. A script
that is valid Lua may be rejected by Grindlang's checker — that is intended.

### Hard rules (the constraint contract)

- **Top-level code runs once, then freezes.** A module's top-level `local`s, loops, `if`s,
  and calls run when it initializes; after that, nothing they built can change — not by a
  call, and not by a closure they built. Only host memory changes between calls (§3.1).
- **No free globals.** The only names in scope are locals, top-level declarations, and
  **host-injected bindings** (§7). There is no implicit global table, no `_G`, no ambient
  stdlib.
- **Numbers are `f64` only.** One numeric type. No integer subtype, no bitwise ops _(v1)_.
- **Statically typed.** Every expression has a type known at compile time (§5).

## 2. Lexical structure

### 2.1 Encoding & whitespace
Source is UTF-8. Whitespace (space, tab, CR, LF, form-feed, vertical-tab) separates
tokens and is otherwise insignificant.

### 2.2 Comments
Identical to Lua:

```lua
-- line comment
--[[ block
     comment ]]
--[==[ block comment with long bracket level 2 ]==]
```

**Doc-comment annotations.** Grindlang reads [EmmyLua](https://luals.github.io/wiki/annotations/)-style
annotations inside `---` line comments to *optionally* pin or document types (§5.6).
Plain Lua highlighters ignore them; the Lua language server renders them as hovers.

```lua
---@param base number
---@param mult number
---@return number
function damage(base, mult)
  return base * mult
end
```

### 2.3 Names (identifiers)
`[A-Za-z_][A-Za-z0-9_]*`, excluding keywords. Names beginning with `_` are permitted.

### 2.4 Keywords (reserved)
```
and    break  do     else   elseif end    false  for
function if    in     local  nil    not    or     repeat
return then    true   until  while
```
Reserved-but-unsupported (tokenized as keywords, but rejected with a "not supported in
Grindlang" diagnostic wherever they appear): `goto` and Lua's label syntax `::name::`;
`repeat` / `until` (the `repeat … until` loop is not supported — use `while`).

### 2.5 Numbers
All numeric literals denote `f64`. Accepted forms (Lua-compatible):
- decimal integer / float: `3`, `3.0`, `0.5`, `.5`, `3.`, `1e3`, `2.5E-4`
- hexadecimal: `0xFF`, `0x1p4` (hex float)

There is no integer literal *type* — `3` and `3.0` are the same value.

### 2.6 Strings
Immutable byte strings. Forms (Lua-compatible):
- quoted: `"..."` and `'...'` with escapes `\n \t \r \\ \" \' \0 \xNN \ddd \u{NNNN}`
- long brackets: `[[ ... ]]`, `[==[ ... ]==]` (no escape processing, leading newline stripped)

### 2.7 Tokens / operators
```
+  -  *  /  //  %  ^  #
==  ~=  <=  >=  <  >
=  (  )  {  }  [  ]
;  :  ,  .  ..  ...
```
`...` is tokenized but only legal where the grammar allows it (it is **not** allowed —
varargs are unsupported _(v1)_; the token is reserved).

## 3. Grammar

Notation: EBNF. `{x}` = zero or more, `[x]` = optional, `|` = alternation, `'x'` =
terminal. `Name`, `Number`, `String` are lexical tokens from §2.

### 3.1 Top level (chunk)

```ebnf
chunk      ::= {topitem} [exportstat]

topitem    ::= funcdecl
             | 'local' 'function' Name funcbody   -- a private function
             | constdecl
             | stat                               -- top-level code (§3.3)

funcdecl   ::= 'function' Name funcbody
constdecl  ::= Name '=' exp

exportstat ::= 'return' tablecons [';']
```

**Top-level code.** A module's `constdecl`s and top-level `stat`s are its **top-level code**,
which runs **once** per module instance, in source order, before the module's first call.
Everything it builds is then **frozen**: no later call can change it, which is what keeps §1's
rule that only host memory survives between calls. It can loop, branch, call functions, and
build tables — a lookup table computed once, for instance:

```lua
local SQUARES = {}
for i = 1, 10 do
  SQUARES[i] = i * i
end

function square(n)
  local v = SQUARES[n]
  if v ~= nil then return v end
  return -1
end
```

- **Declarations.** A `funcdecl`'s and a `constdecl`'s `Name` are **module exports** and
  **immutable bindings**, in scope **everywhere** — above their declaration too, so functions
  may call one another and recurse (including mutual recursion). A top-level `local` declares a
  **module variable**, and `local function` a private function; each is in scope only **from
  its declaration on** (to the end of the chunk, including inside the functions declared after
  it), as in Lua. Top-level names must be distinct (`E0305`); a `local` inside one of top-level
  code's blocks is an ordinary local, and may shadow one.
- **`Name = exp` at the top level** declares a constant, unless `Name` is a top-level `local`
  already in scope, in which case it assigns that module variable, as in Lua. A constant's
  initializer is any expression — names, calls, field reads.
- **Order.** Top-level code runs statement by statement. A constant or module variable read
  before the code that sets it has run — by top-level code calling, too early, a function that
  reads it — is a runtime error. After initialization every one is set, so a call can't hit
  this.
- **What top-level code may do.** Anything a function body may, except `return` (`E0309`; the
  export table is the only top-level `return`) and using host memory (`E0310`): memory is bound
  to each module instance after it initializes, and freezing a value derived from it would only
  capture a snapshot. A function that top-level code calls can't use memory either — that is a
  runtime error. Top-level code may call host functions; what they return is frozen like
  anything else it builds.
- **Only top-level code changes module values.** Top-level code may assign a module variable
  and write through a constant or module variable (`T.x = …`) as it builds them. A function may
  do neither — assigning a module variable in one is `E0308`, and writing through a module value
  is `E0307` — since by the time any call runs, the module has initialized and they are frozen.
  Constants cannot be reassigned at all (`E0302`). Reading is always fine, and writing through
  **memory** stays fine (§7) — the rule keys on what the assignment target is rooted in, not on
  its shape.
- **Freezing.** Once top-level code has run, every table, array, and closure-captured variable
  reachable from a constant or module variable is immutable. The guarantee holds **however the
  value was reached**, not just syntactically: binding one to a local first (`local t = C;
  t.x = …`) gets past `E0307`, which keys on the assignment target's root, and is refused at run
  time instead — as is a write into a *nested* table or array, and a closure kept in a module
  value assigning a variable it captured. Reading through an alias stays fine; only writing is
  refused. A module value is one object for the module's lifetime, so `C == C` holds in and
  across calls (reference values compare by *identity*, §3.5).
- **Errors.** A runtime error in top-level code fails the module: every call reports it
  (`Engine::compile` runs top-level code eagerly, and reports it as a build error). A host
  function that *panics* in top-level code panics the call that ran it, and top-level code runs
  again, from the start, on the next (§7.3).
- If present, the trailing `exportstat` **curates the public surface**: only the names it
  lists are exported, under the keys given. Without it, every `funcdecl` and `constdecl` is
  exported under its own name; a top-level `local` or `local function` is private. (There is
  no `local M = {}` / `return M` idiom — the module table is implicit.) Each entry is
  `key = Name`, where `Name` is a top-level function, constant, or `local`; any other value — a
  literal, an expression, a host function — is an `E0411` error.
- `function M.foo()` style member syntax does **not** exist — all top-level functions are
  already module members.

### 3.2 Function bodies & blocks

```ebnf
funcbody   ::= '(' [parlist] ')' block 'end'
parlist    ::= Name {',' Name}

block      ::= {stat} [retstat]
retstat    ::= 'return' [explist] [';']
```

### 3.3 Statements

Statements appear in function bodies and in top-level code (§3.1), where a top-level `local`
declares a module variable and a top-level `local function` a private top-level function.

```ebnf
stat ::= ';'
       | 'local' namelist ['=' explist]
       | varlist '=' explist
       | functioncall
       | 'do' block 'end'
       | 'while' exp 'do' block 'end'
       | 'if' exp 'then' block {'elseif' exp 'then' block} ['else' block] 'end'
       | numericfor
       | genericfor
       | 'break'
       | 'local' 'function' Name funcbody

numericfor ::= 'for' Name '=' exp ',' exp [',' exp] 'do' block 'end'
genericfor ::= 'for' namelist 'in' iterexpr 'do' block 'end'

namelist   ::= Name {',' Name}
varlist    ::= var {',' var}
explist    ::= exp {',' exp}
```

- Inside a body (or a block of top-level code), `local function` declares a local holding a
  closure, a first-class value. At the top level it declares a private *named* function,
  which, like a `funcdecl`, can only be called directly (§5.5).
- `genericfor`'s `iterexpr` is restricted to the builtins `ipairs(arr)` and `pairs(map)`
  _(v1)_ — there are no user-defined iterators (no metatables). A record is not iterable: its
  fields can have different types, so no one loop variable type fits. Iterating anything else
  is an `E0412` error.
- A `numericfor` evaluates its start, limit, and step once, before the first iteration, and
  counts with a hidden counter. The loop variable receives the counter's value at the start
  of each iteration, so assigning to it inside the body does not change how many times the
  loop runs.
- A `genericfor` visits the indices (`ipairs`) or keys (`pairs`) present **when the loop
  starts**; each element or value is read when the loop reaches it. Writes made earlier in the
  body are therefore observed, but appending to the array or adding a key does not extend the
  loop, so iteration always terminates.

```ebnf
iterexpr ::= 'ipairs' '(' exp ')'
           | 'pairs'  '(' exp ')'
```

### 3.4 Expressions

```ebnf
exp ::= 'nil' | 'true' | 'false'
      | Number | String
      | functiondef
      | prefixexp
      | tablecons
      | exp binop exp
      | unop exp

functiondef ::= 'function' funcbody          -- anonymous closure (§5.5)

prefixexp   ::= var
              | functioncall
              | '(' exp ')'

var         ::= Name
              | prefixexp '[' exp ']'
              | prefixexp '.' Name

functioncall ::= prefixexp args
               | prefixexp ':' Name args      -- method call (host userdata only, §7)

args        ::= '(' [explist] ')'
              | tablecons
              | String

tablecons   ::= '{' [fieldlist] '}'
fieldlist   ::= field {fieldsep field} [fieldsep]
field       ::= '[' exp ']' '=' exp
              | Name '=' exp
              | exp
fieldsep    ::= ',' | ';'
```

### 3.5 Operators — precedence (lowest → highest)

```
or
and
<  >  <=  >=  ~=  ==
..                       -- right-associative, string concat
+  -
*  /  //  %
unary:  not  #  -        -- (unary minus)
^                        -- right-associative, exponent
```

```ebnf
binop ::= '+' | '-' | '*' | '/' | '//' | '%' | '^'
        | '..' | '<' | '>' | '<=' | '>=' | '==' | '~='
        | 'and' | 'or'
unop  ::= '-' | 'not' | '#'
```

Operator typing _(v1)_:
- arithmetic `+ - * / // % ^` : `number × number → number`
- concat `..` : `string × string → string`
- relational `< > <= >=` : `number × number → bool` **or** `string × string → bool`
- equality `== ~=` : both operands the same type → `bool`
- `and` / `or` : yields one of its operands, as in Lua — `a and b` is `a` if `a` is `nil` or
  `false`, else `b`; `a or b` is `a` if `a` is neither, else `b` — typed as described below
- `not` : `bool → bool`
- `#` : `string → number` or `array<T> → number`
- unary `-` : `number → number`

**Equality compares scalars by value and reference types by identity** (Lua semantics).
`nil`, `bool`, `number`, and `string` compare by their value; `array`, `map`/`record`, tuples,
and function values compare by *which object they are*. So two separately built tables with the
same contents are **not** equal, and a table is equal to itself however it was reached —
including through a constant, which top-level code builds once (§3.1).

**`and` / `or` operands may differ in type.** The result's type is the one type that covers
every operand the expression can yield, worked out across a whole chain such as `c and a or b`;
if there is no such type, it is an `E0455` error:

- `x or d` with `x : T?` is a `T` when `d : T` — a default for an optional — and a `T?` when
  `d` is a `T?` or `nil`.
- `x and e` with `x : T?` yields `nil` or `e`, so it is `E?` where `e : E`.
- `c and a or b` with `c : bool` and `a, b : T` is a `T`: one of two values. As in Lua, a `nil`
  or `false` `a` yields `b`. `c and a` alone yields `false` or `a`, so it is an error unless
  `a` is a `bool`.
- Two `bool`s give a `bool`.

The left operand must be one that can be `nil` or `false`: a `bool`, an optional, or `nil`. One
that never is (a number, a string, a table, …) is an `E0456` error, since `and` would always
yield its right operand and `or` never would. An optional operand whose type only later code
settles (a `local x = nil` assigned further down) is taken to be `nil` when falsy; if it turns
out to be a `bool?`, which can also be `false`, that is an `E0455` error.

**Conditions must be `bool`.** Unlike Lua, `if` and `while` require a `bool`; there is no
implicit "everything except `nil`/`false` is truthy" coercion of arbitrary types. `and`/`or`
are the one place that tests for `nil`/`false`, and a condition built with them must still
come out a `bool`: `if x and n > 0` with `x : number?` is a `bool?` and is rejected. Narrowing
an optional uses an explicit comparison: `if v ~= nil then …`.

## 4. Module & export model

```lua
-- exported constant
MAX_LEVEL = 99

-- exported functions; mutually in scope
function xp_for(level)
  return level * level * 10
end

function can_level_up(xp, level)
  return level < MAX_LEVEL and xp >= xp_for(level + 1)
end
```

The module above exports `{ MAX_LEVEL, xp_for, can_level_up }`. A top-level `local` or
`local function` stays private:

```lua
local XP_BASE = 10                     -- private module variable

local function curve(level)           -- private function
  return level * level
end

function xp_for(level)
  return curve(level) * XP_BASE
end
```

To expose a curated subset under chosen keys, end with an export table:

```lua
return {
  next_cost = xp_for,
  ready     = can_level_up,
}
```

Only `next_cost` and `ready` are then visible to the host; `MAX_LEVEL` and the original
names become private to the module.

The host receives the module's **export signature** (each exported name with its inferred
type) so it knows what it can call and with which argument types.

## 5. Type system

### 5.1 Type lattice _(v1)_

```
number          -- f64
bool
string          -- immutable bytes
nil             -- the type of the literal `nil`; only inhabits optionals
T?              -- optional: T or nil
array<T>        -- homogeneous, 1-based, dense
map<string, T>  -- homogeneous string-keyed
record { k1: T1, k2: T2, ... }   -- fixed, known string keys
fn(T1, ..., Tn) -> (R1, ..., Rm) -- functions; m may be 0, 1, or a tuple
userdata<H>     -- opaque host type H (§7)
```

No `any`, no union types (other than `T?`), no user-defined nominal types _(v1)_.

### 5.2 Inference

Types are **inferred**, not annotated, by default. Inference is bidirectional with
unification:
- A `local`'s type comes from its initializer. A `local` initialized to `nil` is optional,
  `T?`, where `T` is the type of what is later assigned to it (§5.3): `local best = nil` can
  go on to hold a record, a number, or a closure. One that is only ever assigned `nil` has type
  `nil`. A `local` with no initializer (`local x`) holds `nil` until assigned, and is typed
  exactly as if initialized to `nil`, so it too must be narrowed before its value is used, even
  where every path assigns it first.
- Inference does not depend on the order of the code: a value may be used above the assignment
  that gives it its type, as when a loop reads what the previous iteration stored (§5.3), and
  the use is checked against that type. A field read, field write, or call on a value that
  nothing ever gives a type is an `E0410` error.
- A parameter's type is inferred from how it is **used** in the body (operators, calls,
  field/index access). For calc-style code this almost always pins the type
  (`base * mult` ⇒ both `number`).
- A function's return type is the unified type of its `return` expressions (or `()` if
  none). When one `return` gives the literal `nil` and another a value of type `T`, it is `T?`
  (§5.5).
- Table literal shape is inferred: all `Name = exp` ⇒ `record`; all positional `exp`
  with one element type ⇒ `array<T>`; `[exp]=exp` with `string` keys and one value type ⇒
  `map<string, T>`. Mixing record and array forms in one literal is a type error.

If a parameter's type cannot be inferred (e.g. it is only stored/passed through) and is
not annotated, compilation fails with an "ambiguous type, add an annotation" diagnostic.

### 5.3 Optionals & narrowing

`nil` is assignable only to an optional slot. A `T?` cannot be used as a `T` until
narrowed:

```lua
function name_or_default(p)        -- p : record{ name: string? }
  if p.name ~= nil then
    return p.name                  -- here p.name : string
  end
  return "unknown"
end
```

A local initialized to `nil` is narrowed the same way before its value is used:

```lua
---@param items { name: string, score: number }[]
function best_name(items)
  local best = nil                 -- best : { name: string, score: number }?
  for _, it in ipairs(items) do
    if best ~= nil then
      if it.score > best.score then best = it end
    else
      best = it
    end
  end
  if best ~= nil then return best.name end
  return "none"
end
```

`or` gives an optional a default without narrowing it: `p.name or "unknown"` is a `string`
(§3.5).

### 5.4 Records, arrays, maps

- **record** keys are known at compile time; `t.k`/`t["k"]` with a literal key are
  checked against the record's fields. Unknown field ⇒ error.
- **array** is 1-based; indexing `a[i]` **always yields `T?`** _(v1)_ and must be narrowed
  before use (no static bounds analysis — chosen for implementation simplicity). The
  per-element binding inside an `ipairs` loop is already `T`, so loops are the ergonomic
  way to read arrays without per-access narrowing.
- **map** indexing `m[k]` with a dynamic `string` key yields `T?`.

### 5.5 Functions, tuples, closures

- A function that returns a value must return one **on every path**. Reaching its closing `end`,
  or a bare `return`, in a function whose return type is not optional is a compile error
  (`E0480`): there would be no value to hand the caller. A function returning an optional (`T?`)
  may do either, and the call then returns `nil`. The check follows the structure of the code:
  an `if` returns on every path only when it has an `else` and every branch returns, a
  `while true` loop with no `break` never finishes, and any other loop may run zero times.
- A function **returns nothing** (its return type is `()`) when none of its `return`s gives a
  value, or when it returns a call to such a function: `return f()` calls `f` and then returns
  no value, as in Lua. The host calling it gets `nil`, as from a bare `return`. In a script, a
  call to a function that returns nothing is **not a value**: besides `return f()`, it may only
  be made as a statement. Using it as a value anywhere else — binding it (`local x = f()`),
  passing it, or as an operand, a condition, or a table or tuple element — is a compile error
  (`E0435`). Lua would make the missing value `nil`; Grindlang doesn't pad a missing value with
  `nil` (below). Whether a function returns nothing is settled from its `return`s, not from how
  its calls are used, so a use above the function is rejected just as one below it is.
- A function whose return type is optional (`T?`) may `return` a `T` or `nil` on any path, as
  an assignment to a `T?` slot may. The type is optional either because `---@return T?` declares
  it (for a tuple return, per element) or because the function returns the literal `nil` on one
  path and a value on another, which infers `T?`. Otherwise the returns must agree exactly: one
  returning a `number?` and another a `number` is an `E0413` mismatch until `---@return number?`
  says which is meant.
- Functions may return multiple values as a **tuple**. A `return e1, …, en` with `n ≥ 2`
  builds a tuple of `n` single-valued elements (a nested multi-value call in the list is **not**
  flattened — `E0415`). The tuple type is written `(T1, …, Tn)` and is inferred from the
  `return` expressions (or pinned by repeating `---@return`, one line per element).
- A tuple is **only** consumable in two positions:
  1. **Parallel binding / assignment** — `local q, r = divmod(a, b)` or `q, r = divmod(a, b)`,
     where the single right-hand call produces exactly as many values as there are targets.
  2. **Pass-through return** — `return divmod(a, b)`, forwarding the callee's tuple unchanged.

  **Arity must match exactly.** Unlike Lua, Grindlang does **not** silently truncate extra
  values or pad missing ones with `nil`; a count mismatch is a compile error (`E0414` for a
  binding, `E0413` for an inconsistent `return`). A tuple used in **any** other position —
  as a single value, an operand, a call argument, an array/table element, or mixed into a
  value list (`local a, b = f(), 5`) — is a compile error (`E0415`). A call made as a
  statement, `divmod(a, b)`, discards whatever it returns, a tuple included; that is not a use
  of the tuple. *(v1 / "Tier A": there is
  no value-count adjustment and no spreading of a call's results into an argument list; those
  Lua behaviors are intentionally omitted.)*
- Anonymous `function … end` expressions (closures) are allowed in function bodies and in
  top-level code; they may capture enclosing locals (their *upvalues*). Captured variables are
  shared mutable cells: a write through one closure is observed by the enclosing scope and by
  sibling closures. A module variable is not captured — every function reads it directly — and
  a closure kept in a module value is frozen with it: it may read what it captured, but
  assigning it is a runtime error (§3.1).
- A closure is a **first-class value**: it may be returned (including across the host
  boundary) and held by the host, which can re-invoke it later — multiple times, observing
  mutations to its upvalues between calls (`Module::call_value`). A returned closure keeps its
  backing code and captured state alive for as long as the host holds it.
- A **named** top-level function (a `function` or top-level `local function`) or host function
  is not a value: it may only be called directly, `f(args)`. Binding, returning, storing, or passing it (`local g = f`) is an `E0434` error. To
  use one as a value, wrap the call in a closure: `local g = function(x) return f(x) end`.
- A closure is **bound to the module instance that created it**. Only that instance may invoke
  it; passing it to a different `Module` — even one compiled from the identical source — is a
  runtime error, as is a script receiving a foreign closure (via a host function) and calling
  it. A closure is not a portable value: it names a compiled body that only means anything
  inside its own module, alongside that module's constants and host bindings. Holding one after
  its module is dropped stays safe (the code stays mapped) but it can no longer be invoked, so
  a host that intends to call a closure must keep its `Module` alive.
- A closure the host hands a script — as a host function's result, say — must have the function
  type the script expects of it. The JIT calls a closure through a native signature fixed by
  that type, so calling one whose parameters or result are represented differently (a different
  number of parameters, or a `number` or `bool` on one side and any other type on the other) is
  a runtime error at the call, not a call that passes arguments the closure can't read. The
  interpreters check nothing at the call, and fail later, if at all, where a value contradicts
  its type.
- A closure still **cannot be persisted** into host memory or serialized — that would require
  capturing native-code identity and live captured state; closures are confined to the
  in-process value graph.

### 5.6 Annotations (optional)

EmmyLua `---@` comments (§2.2) may pin types where inference is insufficient (e.g. a
parameter used only by pass-through, or a record/array parameter whose shape inference can't
recover) or for documentation. A `---` doc-comment block **immediately preceding** a
top-level `function` or `local function` annotates it; the block binds to that function only.

Recognized _(v1)_: `---@param <name> <type>` and `---@return <type>` on top-level functions.
A single `---@return` pins a single return type; **two or more `---@return` lines** pin a
multi-value (tuple) return in order (§5.5). `---@type <type>` (locals) is **deferred**. Other
EmmyLua directives (`@class`, `@field`, …) and free-form doc text are ignored, so a file stays
valid EmmyLua documentation; only a malformed `@param`/`@return` we consume is an error.

**Accepted type syntax _(v1)_** — the EmmyLua spelling of the §5.1 lattice:

```ebnf
type    ::= primary { '?' | '[' ']' }          -- postfix optional / array, left-to-right
primary ::= 'number' | 'bool' | 'string' | 'nil'
          | '{' '[' 'string' ']' ':' type '}'  -- map<string, T>
          | '{' field {',' field} [','] '}'    -- record
field   ::= Name ':' type
```

Postfix binds left-to-right: `number[]?` is an optional array; `number?[]` is an array of
optionals. Record and map literals may nest. Function types (`fn(...)->...`) and host
`userdata` are **not** yet spellable in an annotation.

An annotation is applied **before** the body is checked, so a record/array parameter's shape
is visible to the body. An annotation that **contradicts** how the body uses the value is a
compile error at the conflicting use (e.g. `---@param x string` with `x + 1` fails the numeric
operator). `---@param` naming a non-parameter, or an unknown type name, is also an error.

## 6. Builtins _(v1)_

A small, pure, deterministic set — no global namespace pollution beyond these names:

- **math** (as a record-like namespace): `math.floor`, `math.ceil`, `math.abs`,
  `math.min`, `math.max`, `math.sqrt`, `math.pow`, `math.log10`, `math.huge`, `math.pi`. All
  operate on `number`. `math.log10` is base-10 logarithm; like `math.pow` it has no native
  cranelift instruction, so the JIT routes it through the shared reference impl (`f64::log10`),
  keeping the three oracles bit-identical.
- **string**: `string.len`, `string.sub`, `string.upper`, `string.lower`,
  `string.find` _(plain, no patterns in v1)_, `string.format` _(restricted verbs)_.
- **iteration**: `ipairs(array<T>)`, `pairs(map<string,T>)` — usable only in
  `genericfor` (§3.3).
- **conversion**: `tostring(x)` → `string` for `number`/`bool`/`string`; `tonumber(s)` →
  `number?`.

No `print`, `io`, `os`, `require`, `load`, `pcall`, `setmetatable`, `coroutine`, or
`random`/time. Capabilities like randomness or game queries are **host-injected** (§7).

## 7. Host interop

The host (Rust) injects two kinds of bindings, both in scope at the top level and in all
function bodies:

1. **Registered functions** — host-provided functions with declared signatures, callable
   like any Grindlang function. These are how a script reaches game state, RNG, lookups,
   etc.
2. **Memory** — a userdata-like value (`userdata<H>` for a host type `H`) representing
   **persistent, Rust-owned state** that survives between invocations. Its fields/methods
   are declared by the host's schema and compile to **direct Rust calls** (no copying for
   reads where possible). Field access (`mem.gold`) and method calls (`mem:add_item(id)`)
   are the only places `:`-method syntax is valid _(v1)_ — see §7.2.

```lua
---@param amount number
function spend_gold(amount)
  if mem.gold >= amount then
    mem.gold = mem.gold - amount      -- writes persist via host memory
    return true
  end
  return false
end
```

The names of injected bindings (`mem`, registered functions) are configured per embedding;
`mem` is used illustratively. Injected names are reserved within a script — a top-level
declaration may not shadow them.

Top-level code (§3.1) may call registered functions but not use memory, which a host binds to
each module instance after the module has initialized: `E0310` in top-level code itself, and a
runtime error in a function top-level code calls.

A binding that the embedding **declares but never provides** is a runtime error the first time
a script reads it — never a silent `nil`. The check is lazy: declaring a memory the script
never touches costs nothing, and a script that reads it fails at that point (§7.3) rather than
at call entry. Binding a memory explicitly *to* `nil` is a different thing: the host provided a
value, so the read succeeds. The same rule applies to a declared host function that was never
registered, and to a declared memory method (§7.2).

### 7.1 Calling exports from the host (arity & marshaling)

The host invokes a compiled module's exports with Rust values and marshals the result back to
a requested Rust type (`Module::call` / `call_typed`). Three boundary rules mirror the
language's in-script discipline rather than silently coercing:

- **Exact arity.** A call must supply exactly as many arguments as the export declares — the
  same rule §5.5 enforces in-script, now enforced at the host boundary too. A mismatch is a
  call error, not a silent pad-with-`nil` / drop-surplus. The rule covers host-invoking a
  returned closure (`Module::call_value`) as well, checked against the closure's *source-level*
  parameters — its captured environment is not one of them. Reading a constant through the same
  entry point takes no arguments at all, so passing any is likewise an error.
- **Argument types.** An argument's runtime shape must match the parameter's declared type. A
  string passed where `number` is declared is a call error, not a silent `0`. The check happens
  at the boundary because a scalar argument is converted to raw bits on the way in, leaving
  nothing downstream to distinguish a wrong value from a plausible one. Values the host
  supplies *elsewhere* — a memory binding, a registered function's result — keep their identity
  further in and are instead rejected at the point where a script forces them into a scalar
  (§7.3), which is where the interpreters check too.
- **Integer marshaling.** Numbers are `f64`; marshaling a result into an integer Rust type
  (`i32`/`i64`/`u32`/`usize`) **truncates a finite non-integral value toward zero** — scripts
  are trusted dev code, so a fractional formula is treated as intentional — but **rejects a
  non-finite value** (`NaN`, `±∞`) as a marshaling error rather than letting it land silently
  as `0` / `i*::MAX` via a saturating cast. Marshaling into `f64`/`f32` passes any value
  through unchanged (non-finite floats are valid).

### 7.2 Memory methods (`mem:method(args)`)

A memory binding may expose **methods** — host-provided behavior invoked with Lua's
colon-call syntax. `mem:add_item(id)` calls the host's `add_item` implementation, which
receives the receiver as an implicit `self` (its **first** argument) followed by the call
arguments. This is the only place `:`-call syntax is accepted _(v1)_.

- **Declaration.** Each method is registered on its memory binding with a signature whose
  parameters are the **call arguments** — the receiver is implicit and typed by the memory
  binding, so it is *not* listed in the declared parameters. The result type is the method's
  return type.
- **Receiver.** The receiver must be a host-memory binding referenced **directly by name**
  (`mem:m(...)`), not an arbitrary expression — a `:` call on a non-memory receiver is a
  `E0417` error. (In practice scripts cannot construct method-bearing values themselves, so
  this confines methods to host memory, matching §7.)
- **Arity.** Exactly as many arguments as the declared signature — no pad/drop (§5.5). A
  mismatch is `E0433`; an unknown method on the memory is `E0432`.
- **Semantics.** A method is a **direct Rust call**; it may read and mutate the receiver's
  Rust-owned state, and those writes persist between invocations like any memory write.

```lua
-- `mem` is host memory exposing a method `add_item(id: number) -> bool`
---@param id number
function pick_up(id)
  return mem:add_item(id)      -- host mutates persistent inventory state, returns success
end
```

### 7.3 Runtime errors

Static typing rules out most failures, but a few survive to run time: an out-of-range array
write, ordering a `NaN`, an unbound memory read, calls nested too deeply, or an error returned by
a host function or method.

- **A runtime error aborts the call.** Evaluation stops at the failing operation. No later
  statement in that function runs, no enclosing loop takes another iteration, and the error
  propagates out through every caller — a script function that calls a failing one does not
  continue either. The host sees `Err`; there is no in-script way to observe or recover from a
  runtime error (`pcall` is not in the language, §8).
- **The first error wins.** If an error is raised while one is already pending, the original is
  the one reported.
- **Effects already applied persist.** A call that fails partway leaves any host memory it
  already wrote in its mutated state — there is no rollback. Only effects *before* the failing
  operation are visible; effects that would have followed it never happen. Hosts that need
  all-or-nothing updates must stage them (e.g. write through a method that commits at the end).
- **Call depth is limited.** At most 200 script calls may be active at once — the host's own
  call, nested calls, and recursion all count — and the call that would exceed that fails with a
  runtime error. A host may set a different limit per module. Script calls run on the calling
  thread's native stack, so without a limit runaway recursion would overflow it and abort the
  host process instead of returning `Err`. The limit counts calls rather than bytes so that every
  backend fails at the same call. Calling a host function or method and reading a constant are
  not script calls and do not count; nor is running top-level code (§3.1), though the functions
  it calls are.
- **A host panic panics the call.** A host function or method that panics is not a runtime
  error: the panic propagates out of the host's call unchanged, in every backend. It stops the
  script where it happened, like an error — nothing after it runs, and effects before it
  persist — and the module stays usable for later calls if the host catches the panic.

These are the semantics every backend implements; they are what the three-oracle invariant
(`Interpreter == Vm == JitModule`) is checked against, side effects included.

## 8. Rejected constructs (diagnostics, not silent)

Each produces a targeted, span-pointing error:

- a function assigning a module variable (`E0308`) or writing through a module value (`E0307`);
  `return` in top-level code (`E0309`); host memory in top-level code (`E0310`) — §3.1
- `local M = {} … return M` idiom (use implicit exports)
- free global read/write; `_G`, `_ENV`
- `goto` / labels; varargs `...`; `repeat … until` loops
- `setmetatable`/`getmetatable`, metatable-driven behavior
- `require`, `load`, `dofile`, `loadstring`, `pcall`/`error`-based control flow _(v1)_
- truthiness coercion (non-`bool` condition; an `and`/`or` whose left operand is never `nil` or
  `false`); mixed-type `==`; heterogeneous tables
- a call to a function that returns nothing, used as a value (`E0435`) — §5.5
- coroutines; integer/bitwise operations
- source nested more than 200 levels deep — expressions, blocks, and function bodies inside one
  another (`E0209`). A long operator chain (`a + b + c + …`) or suffix chain (`t.a.b…`, `f()()`)
  counts one level per link. Every later phase walks the tree recursively, so a deeper tree would
  overflow the native stack. A `---@` type annotation may nest 200 levels too (`E0110` past it).

## 9. Worked examples

### 9.1 A stat calculation module
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
Exports: `{ ARMOR_K: number, mitigated: fn(number, number) -> number,
lethal: fn(number, number, number) -> bool }`.

### 9.2 A dialog-tree decision module
```lua
-- `mem` is host memory: record{ reputation: number, met_elder: bool }

---@return string
function elder_greeting()
  if not mem.met_elder then
    mem.met_elder = true
    return "intro"
  end
  if mem.reputation >= 50 then
    return "warm"
  end
  return "neutral"
end

function choices()
  local out = { "ask_quest", "leave" }
  if mem.reputation >= 50 then
    out[#out + 1] = "ask_favor"
  end
  return out                 -- array<string>
end
```
The host calls `elder_greeting()` to pick a dialog node and `choices()` to build the menu;
both read/write persistent memory.

---

### Resolved decisions (carried into v1)
- **Conditions must be `bool`** — no truthiness coercion (§3.5, §5.3).
- **`and`/`or` operands may differ in type** (§3.5): the result has the one type that covers
  each operand it can yield, so `x or default` gives an optional a default and `c and a or b`
  picks one of two values. Their left operand must be able to be `nil` or `false`, which keeps
  them from testing the truthiness of a value that is always truthy.
- **Array indexing always yields `T?`** — no static bounds analysis; narrow per-access or
  iterate with `ipairs` (§5.4).
- **`repeat … until` is not supported**; **`//` floor-division is supported** (§3.3, §3.5, §8).
- **Multi-return is "Tier A"** (§5.5): a `return`-list builds a tuple, consumed only by a
  parallel binding/assignment or a pass-through `return`, with **exact arity** (no Lua-style
  truncate/pad, no spreading into argument lists, no value-count adjustment). A multi-value
  call anywhere else is a compile error, except as a statement, which discards its results.
- **A call that returns nothing is not a value** (§5.5): it may be a statement or returned with
  `return f()`, and anywhere else is a compile error rather than Lua's `nil` — Tier A's
  no-padding rule applied to zero values. A function returns nothing when none of its `return`s
  gives one, decided before any use of it is checked, so the order of the code can't change it.
- **Method calls are memory-only, self-passing** (§7.2): `mem:m(args)` is valid only when the
  receiver is a host-memory binding named directly; the receiver is passed as an implicit
  `self` to the host method, which resolves to a direct Rust call. `:` on any other receiver
  is a compile error.
- **A local initialized to `nil` is optional** (§5.2): `local x = nil` is `T?`, with `T`
  inferred from what is later assigned to it, rather than `nil`, which no other value fits.
  It is narrowed with `~= nil` like any optional. An uninitialized `local x` is the same, since
  it holds `nil` until assigned; there is no definite-assignment analysis to exempt a local
  that every path assigns before reading.
- **`---@` annotation type-syntax subset** is fixed (§5.6): `@param`/`@return` on top-level
  functions, scalar/optional/array/map/record spellings, `@type` and function/userdata types
  deferred, unknown directives ignored.
- **Top-level code runs once, then freezes** (§3.1) — Starlark's model, chosen over running it
  per call (which would leave top-level locals out of reach of the declared functions) and over
  true Luau (mutable module state, which §1 reserves for host memory). Scoping is Lua's: a
  `function` or constant is in scope everywhere, a top-level `local` from its declaration on,
  and a read before the code that sets it has run is a runtime error. Top-level code may call
  host functions but not use memory. `local` makes a top-level name private. A constant's
  initializer may be any expression; constants are built once and frozen like any other module
  value (they used to be literal-only, and rebuilt per call).

### Open spec items still to settle during implementation
- `string.format` verb whitelist and `string.find` plain-vs-pattern policy (§6).
