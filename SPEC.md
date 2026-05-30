# Grindlang — Language Specification (draft)

> Companion to [PLAN.md](PLAN.md). This document defines the **lexical structure**,
> **grammar**, **module model**, and the **type/interop rules** needed to read the
> grammar. It is the authoring contract for `.lua`-syntax Grindlang scripts.
>
> Status: draft. Sections marked _(v1)_ are the initial target; _(later)_ items are
> deliberately deferred.

## 1. What a Grindlang program is

A Grindlang script is **not** a runnable program. It is a **module definition**: it
evaluates, at compile time, to a **table of exported functions and constants**. The host
compiles a script once into a `Module`, then calls its exported functions many times.
The only state that survives between calls is **host-provided memory** (Rust-owned,
exposed as a userdata-like value — see §7).

Grindlang reuses **Lua's surface syntax** so existing Lua syntax highlighters and LSPs
work on it, but it is **statically typed** and a **constrained subset** of Lua. A script
that is valid Lua may be rejected by Grindlang's checker — that is intended.

### Hard rules (the constraint contract)

- **No top-level local variables.** The top level contains only export declarations
  (§4). Mutable variables (`local x = …`) are legal **only inside function bodies**.
- **No free globals.** The only names in scope at the top level are other top-level
  declarations and **host-injected bindings** (§7). There is no implicit global table,
  no `_G`, no ambient stdlib.
- **No top-level executable statements.** The top level is a set of *declarations*, not a
  statement sequence; nothing "runs" at load time beyond evaluating constant expressions.
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
chunk      ::= {topdecl} [exportstat]

topdecl    ::= funcdecl
             | constdecl

funcdecl   ::= 'function' Name funcbody
constdecl  ::= Name '=' constexpr

exportstat ::= 'return' tablecons [';']
```

- A `funcdecl`'s `Name` and a `constdecl`'s `Name` are **module exports** and are
  **immutable bindings**. All top-level names are mutually in scope, so functions may
  call one another and recurse (including mutual recursion).
- `constexpr` is a **compile-time-evaluable** expression: literals, `tablecons` of
  constants, and `unop`/`binop` over constants. In v1 a `constexpr` may **not** reference
  names (not even other top-level constants), call functions, or index/field-access —
  only literal values and operators over them. (This avoids const-ordering and cycle
  analysis; revisit if a consumer needs constant folding across declarations.)
- If present, the trailing `exportstat` **curates the public surface**: only the names it
  lists are exported, under the keys given. Without it, *all* top-level declarations are
  exported under their own names. (There is no `local M = {}` / `return M` idiom — the
  module table is implicit.)
- `function M.foo()` style member syntax does **not** exist — all top-level functions are
  already module members.

### 3.2 Function bodies & blocks

```ebnf
funcbody   ::= '(' [parlist] ')' block 'end'
parlist    ::= Name {',' Name}

block      ::= {stat} [retstat]
retstat    ::= 'return' [explist] [';']
```

### 3.3 Statements (function-body only)

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

- `local function` is allowed **inside bodies** (it is a local, which is legal here, not
  at the top level).
- `genericfor`'s `iterexpr` is restricted to the builtins `ipairs(arr)` and `pairs(map)`
  / `pairs(record)` _(v1)_ — there are no user-defined iterators (no metatables).

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

functiondef ::= 'function' funcbody          -- anonymous closure (in-call only, §5.5)

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
- `and` / `or` : both operands the same type `T` → `T` (no truthiness coercion across
  types; condition positions require `bool` — see below)
- `not` : `bool → bool`
- `#` : `string → number` or `array<T> → number`
- unary `-` : `number → number`

**Conditions must be `bool`.** Unlike Lua, `if`, `while`, and the operands of `and`/`or`
in condition position require a `bool`; there is no implicit "everything except
`nil`/`false` is truthy" coercion of arbitrary types. Narrowing an optional uses an
explicit comparison: `if v ~= nil then …`.

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

The module above exports `{ MAX_LEVEL, xp_for, can_level_up }`. To expose a curated
subset under chosen keys, end with an export table:

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
- A `local`'s type comes from its initializer.
- A parameter's type is inferred from how it is **used** in the body (operators, calls,
  field/index access). For calc-style code this almost always pins the type
  (`base * mult` ⇒ both `number`).
- A function's return type is the unified type of its `return` expressions (or `()` if
  none).
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

### 5.4 Records, arrays, maps

- **record** keys are known at compile time; `t.k`/`t["k"]` with a literal key are
  checked against the record's fields. Unknown field ⇒ error.
- **array** is 1-based; indexing `a[i]` **always yields `T?`** _(v1)_ and must be narrowed
  before use (no static bounds analysis — chosen for implementation simplicity). The
  per-element binding inside an `ipairs` loop is already `T`, so loops are the ergonomic
  way to read arrays without per-access narrowing.
- **map** indexing `m[k]` with a dynamic `string` key yields `T?`.

### 5.5 Functions, tuples, closures

- Functions may return multiple values (a tuple), consumed by parallel assignment:
  `local q, r = divmod(a, b)`. **Arity must match exactly:** the number of values a call
  produces must equal the number of targets (in assignment) or the callee's declared
  parameter count (when used as a call argument). Unlike Lua, Grindlang does **not**
  silently truncate extra values or pad missing ones with `nil` — a mismatch is a compile
  error.
- Anonymous `function … end` expressions (closures) are allowed **inside** function
  bodies; they may capture enclosing locals. They live in the per-call arena and **cannot
  be persisted** across invocations or stored into host memory.

### 5.6 Annotations (optional)

EmmyLua `---@` comments (§2.2) may pin types where inference is insufficient or for
documentation. Recognized _(v1)_: `---@param <name> <type>`, `---@return <type>`,
`---@type <type>`. Annotation type syntax mirrors §5.1 in EmmyLua spelling
(`number`, `string?`, `number[]` for `array<number>`, `{ [string]: T }` for `map`,
table literal for records). An annotation that contradicts inference is a compile error.

## 6. Builtins _(v1)_

A small, pure, deterministic set — no global namespace pollution beyond these names:

- **math** (as a record-like namespace): `math.floor`, `math.ceil`, `math.abs`,
  `math.min`, `math.max`, `math.sqrt`, `math.pow`, `math.huge`, `math.pi`. All operate on
  `number`.
- **string**: `string.len`, `string.sub`, `string.upper`, `string.lower`,
  `string.find` _(plain, no patterns in v1)_, `string.format` _(restricted verbs)_.
- **iteration**: `ipairs(array<T>)`, `pairs(map<string,T> | record)` — usable only in
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
   are the only places `:`-method syntax is valid _(v1)_.

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

## 8. Rejected constructs (diagnostics, not silent)

Each produces a targeted, span-pointing error:

- top-level `local` / any top-level statement that isn't a `funcdecl`/`constdecl`/export
- `local M = {} … return M` idiom (use implicit exports)
- free global read/write; `_G`, `_ENV`
- `goto` / labels; varargs `...`; `repeat … until` loops
- `setmetatable`/`getmetatable`, metatable-driven behavior
- `require`, `load`, `dofile`, `loadstring`, `pcall`/`error`-based control flow _(v1)_
- truthiness coercion (non-`bool` condition); mixed-type `==`; heterogeneous tables
- coroutines; integer/bitwise operations

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
- **Array indexing always yields `T?`** — no static bounds analysis; narrow per-access or
  iterate with `ipairs` (§5.4).
- **`repeat … until` is not supported**; **`//` floor-division is supported** (§3.3, §3.5, §8).
- **Multi-return arity mismatch is a compile error** — no Lua-style truncate/pad (§5.5).

### Open spec items still to settle during implementation
- Exact EmmyLua type-syntax subset accepted by `---@` annotations (§5.6).
- `string.format` verb whitelist and `string.find` plain-vs-pattern policy (§6).
