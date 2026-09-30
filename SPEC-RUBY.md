# Grindlang — Ruby Surface Specification (draft)

> Companion to [SPEC.md](SPEC.md) and [PLAN.md](PLAN.md) Phase 10. SPEC.md defines what a
> Grindlang program **means**; this document defines only the **Ruby spelling** of it: which
> Ruby forms are accepted and which SPEC.md construct each one becomes. Where this document is
> silent, SPEC.md governs.
>
> Status: **draft, not implemented.** The decisions below were confirmed with the stakeholder
> on 2026-09-30; details marked _(open)_ are still to settle. Items that depend on a core
> change name it (C2–C9, PLAN.md Phase 10).

## 1. What a Ruby-surface script is

A `.rb` Grindlang script is a module definition with exactly the model of SPEC.md §1 and §4:
it evaluates to a table of exported functions and constants, its top-level code runs **once**
and everything it builds is then **frozen**, and only host memory survives between calls.

The Ruby surface is a **strict subset of Ruby's syntax**, not a Ruby implementation. There are
no objects, classes, or exceptions; types are static and inferred (SPEC.md §5); numbers are
`f64`; conditions are `bool`. A construct outside this document is rejected with a
"not supported in Grindlang" diagnostic that names it — never a generic parse error.

The host picks the surface: `Engine::compile_ruby` in the embedding API, and in the CLI the
`.rb` extension (overridden by `--surface`). The IR cache key includes the surface.

### 1.1 Differences from Ruby

| Ruby | Ruby surface |
|---|---|
| Every method returns its last value | Only an **expression** in tail position is returned. An assignment, memory write, or loop in tail position returns nothing (§4.2). |
| Integers and floats | `f64` only: `7 / 2` is `3.5`; `.div` floors (§4.7). |
| Everything but `nil`/`false` is truthy | Conditions must be `bool`. `if user` with `user` optional is an error; write `unless user.nil?` (§4.8). |
| `"a" + "b"` | Rejected; use interpolation, `"#{a}#{b}"` (§4.7). |
| `a[-1]` | Not supported. A literal negative index is an error; a computed one reads `nil`. Use `a.last` (§4.9). |
| Constants are mutable unless frozen | Everything top-level code builds is frozen once it has run; `.freeze` is accepted and does nothing (§4.1). |
| `return a, b` returns an array | Returns a tuple, consumed by `a, b = f()` (SPEC.md §5.5). |
| Methods are callable from anywhere, locals are not | Same: a `def` can't read a top-level lowercase local (§4.1). |
| Classes, modules, `self`, `yield`, exceptions | Not supported (§6). |

## 2. Lexical structure

### 2.1 Encoding, whitespace, line breaks
Source is UTF-8. A **newline ends a statement**, as does `;`. A line continues onto the next
when it ends with a binary operator, `,`, `(`, `[`, `{`, `|`, or `\`, or when the next line
begins with `.` (a leading-dot method chain). Other whitespace is insignificant, except where
§3.4 restricts calls without parentheses.

### 2.2 Comments
`#` to end of line. `=begin`/`=end` block comments and `__END__` are rejected.

**Doc-comment annotations.** `# @param <name> <type>` and `# @return <type>` lines immediately
above a `def` pin types exactly as SPEC.md §5.6's `---@` annotations do, with the **same type
syntax** (`number`, `T?`, `T[]`, `{ [string]: T }`, `{ k: T }`):

```ruby
# @param base number
# @param mult number
# @return number
def damage(base, mult)
  base * mult
end
```

### 2.3 Names
- **Locals and methods:** `[a-z_][A-Za-z0-9_]*`, excluding keywords. _(open)_ Method names
  ending in `?` or `!` (`def alive?`) are rejected in v1; see Open items.
- **Constants:** `[A-Z][A-Za-z0-9_]*`. A name's first letter decides whether it is a constant
  (§4.1).
- **Memory fields:** `@name` (§4.10).
- **Symbols:** `:name`, which is the string `"name"` (§4.6).
- Rejected: `$globals`, `@@class_vars`, `:"quoted symbols"`.

### 2.4 Keywords
```
and    break  case   def    do     else   elsif  end    false  if
loop   next   nil    not    or     private public return then  true
unless until  when   while
```
`loop`, `private`, and `public` are methods in Ruby; here they are fixed forms.
Reserved and rejected wherever they appear: `class`, `module`, `self`, `super`, `yield`,
`begin`, `rescue`, `ensure`, `retry`, `redo`, `raise`, `alias`, `undef`, `defined?`, `for`
(use `.each`), `BEGIN`, `END`, `__method__`.

### 2.5 Numbers
All numeric literals are `f64`: `3`, `3.0`, `0.5`, `1e3`, `2.5e-4`, `0xFF`, with `_` allowed
between digits (`1_000`). Rejected: `0b`/`0o` literals, rationals (`3r`), imaginaries (`2i`).

### 2.6 Strings
- `'…'`: raw, with only `\\` and `\'` escapes, as in Ruby.
- `"…"`: the escapes of SPEC.md §2.6, plus interpolation `#{expr}` (§4.6).
- Rejected: heredocs, `%w[]`, `%i[]`, `%q`, `%Q`, `?a` character literals, backticks, regex
  literals.

### 2.7 Operators
```
+  -  *  /  %  **
==  !=  <  <=  >  >=
&&  ||  !  and  or  not
=  +=  -=  *=  /=  %=  **=  ||=  &&=
..  ...  ?  :  ->  .  =>  <<
(  )  [  ]  {  }  |  ,  ;
```
Rejected: `<=>`, `===`, `=~`, `&.`, `::`, and the bitwise operators `& | ^ ~ >>` (SPEC.md §1:
no bitwise operations). `<<` is accepted only as array append (§4.9).

## 3. Grammar

Notation as in SPEC.md §3. `sep` is a newline or `;`.

### 3.1 Top level
```ebnf
program   ::= {topitem sep}
topitem   ::= ['private'] def
            | 'private' | 'public'          -- switch the visibility of later defs
            | stat
def       ::= 'def' Name ['(' [params] ')'] body 'end'
params    ::= Name {',' Name}
body      ::= {stat sep}
```

### 3.2 Statements
```ebnf
stat ::= expr [modifier]
       | assign [modifier]
       | command [modifier]
       | 'if' expr ['then'] body {'elsif' expr ['then'] body} ['else' body] 'end'
       | 'unless' expr ['then'] body ['else' body] 'end'
       | 'case' expr {'when' expr {',' expr} ['then'] body} ['else' body] 'end'
       | 'while' expr ['do'] body 'end'
       | 'until' expr ['do'] body 'end'
       | 'loop' block
       | 'break' | 'next'
       | 'return' [expr {',' expr}]

modifier ::= ('if' | 'unless' | 'while' | 'until') expr
assign   ::= target {',' target} '=' expr {',' expr}
           | target opassign expr
target   ::= Name | Const | Ivar | primary '[' expr ']' | primary '.' Name
opassign ::= '+=' | '-=' | '*=' | '/=' | '%=' | '**=' | '||=' | '&&='
```

### 3.3 Expressions
```ebnf
expr     ::= expr binop expr | unop expr
           | expr '?' expr ':' expr
           | primary

primary  ::= 'nil' | 'true' | 'false' | Number | String | Symbol
           | Name | Const | Ivar
           | '(' expr ')'
           | array | hash | lambda | range
           | call
           | primary '[' expr ']'
           | primary '.' Name                     -- field read or method (§4.11)
           | 'if' … 'end' | 'unless' … 'end' | 'case' … 'end'   -- as values (§4.4)

call     ::= Name '(' [args] ')' [block]
           | primary '.' Name '(' [args] ')' [block]
           | primary '.' Name block
           | primary '.' '(' [args] ')'           -- calling a closure: f.(x)
args     ::= expr {',' expr}
block    ::= 'do' ['|' params '|'] body 'end'
           | '{' ['|' params '|'] body '}'
lambda   ::= '->' ['(' [params] ')'] ('{' body '}' | 'do' body 'end')
           | 'lambda' block
array    ::= '[' [expr {',' expr} [',']] ']'
hash     ::= '{' [pair {',' pair} [',']] '}'
pair     ::= Name ':' expr | expr '=>' expr
range    ::= '(' expr ('..' | '...') expr ')'   -- only as an iteration receiver (§4.5)
```

### 3.4 Calls without parentheses
```ebnf
command  ::= Name cmdarg {',' cmdarg} [block]
cmdarg   ::= expr | Name ':' expr               -- `key: value` only for DSL keywords (§5)
```
A call may omit its parentheses **only at the start of a statement** (`say "Hi"`,
`on :hit do … end`), and its first argument may not begin with `-`, `[`, `(`, `*`, `&`, or
`{`; write `foo(-1)`, not `foo -1`. A `{` directly after a call always opens a **block**, so a
hash argument needs parentheses: `foo({ a: 1 })`. These rules remove Ruby's
whitespace-dependent parses. A bare name with no arguments (`reset`) is a zero-argument call
unless a local of that name is in scope (§4.3).

### 3.5 Precedence (lowest → highest)
```
not                       -- keyword form
and  or                   -- equal precedence, left-associative, as in Ruby
?  :                      -- right-associative
||
&&
==  !=
<  <=  >  >=
+  -
*  /  %
-                         -- unary minus
**                        -- right-associative; binds tighter than unary minus: -2 ** 2 == -4
!                         -- unary
```
Modifiers (`x if c`) and assignment bind looser than every operator. The translator builds
the tree by these rules, so Ruby's `a or b and c` is `(a or b) and c` even though SPEC.md's
Lua spelling ranks `and` above `or`.

## 4. How Ruby maps onto Grindlang

### 4.1 Top level
| Ruby | Becomes |
|---|---|
| `MAX_HP = 100` | the constant `MAX_HP`, exported (SPEC.md §3.1) |
| `table = {}` (lowercase) | a top-level `local`: a private module variable |
| `def name … end` | an exported top-level function |
| `private def name … end`, or a `def` after a bare `private` | a top-level function **not** exported |

Private functions stay visible to the whole file, as Ruby methods are: they become plain
top-level functions, and the translator generates an export table (SPEC.md §4) listing only
the public `def`s and constants. `public` switches later `def`s back.

A `def` that reads a top-level **lowercase** local is an error, as in Ruby, where `def` opens a
new scope; constants are the values functions share. Blocks and lambdas at the top level may
read top-level locals, since they are closures. `CONST.freeze` is accepted and does nothing:
top-level values are frozen after initialization regardless (SPEC.md §3.1). Everything else
about top-level code is SPEC.md's: it may loop, branch, and call host functions; it may not
`return` (E0309) or use host memory (E0310).

### 4.2 Methods and implicit return
A `def`'s parameters are plain names: no defaults, splats, keyword parameters, or `&block`.

The **last statement** of a `def` or lambda body decides what it returns:
- an **expression** (a value, an operator, a call, `?:`, an `if`/`case` used as a value)
  becomes `return expr`. A call to a function that returns nothing is allowed here: the
  function then returns nothing too (SPEC.md §5.5, `return f()`);
- an `if`, `unless`, or `case` statement passes the rule into each of its branches;
- anything else — an assignment, a memory write (`@hp -= 1`), a loop — returns nothing.

Reaching the end of a function that returns a value on some other path returns `nil`, making
its return type optional (C4), as Ruby does. So `def f(c) if c then 1 end end` returns
`number?`. Explicit `return` works as in SPEC.md.

A DSL block's return follows its declared signature instead (§5): a block declared to return
nothing never gets an implicit `return`.

### 4.3 Local variables
A Ruby local is declared by its first assignment. The translator finds each variable's owning
scope — a `def`, a block or lambda, or the top level — and:
- emits `local x = …` at the first assignment when that assignment comes before every use of
  `x` in the same block;
- otherwise emits a bare `local x` at the start of the owning scope. It holds `nil` until
  assigned and is typed as optional (SPEC.md §5.2), matching Ruby, where a variable assigned in
  only one branch is `nil` on the others.

A block sees its enclosing scope's locals; a variable first assigned inside a block is local to
that block. Block parameters shadow outer names.

### 4.4 Control flow
| Ruby | Becomes |
|---|---|
| `if`/`elsif`/`else`, `unless`/`else` | `if`/`elseif`/`else`, with `unless c` as `if not c` |
| `stmt if c`, `stmt unless c` | a one-branch `if` |
| `case x when a, b … when c … else … end` | an `if` chain testing `x == a or x == b`, … (`x` evaluated once) |
| `when lo..hi`, `when lo...hi` | `x >= lo and x <= hi`, `x >= lo and x < hi` |
| `while c`, `until c`, `stmt while c` | `while c`, `while not c` |
| `loop do … end` | `while true do … end` |
| `break`, `next` | `break`, `continue` (C3) |
| `c ? a : b`; `if`/`unless`/`case` used as a value | a conditional expression (C2) |

An `if` or `case` used as a value may have branches of several statements only when it is the
whole right side of an assignment or a `return`; elsewhere each branch must be one expression.
`c ? a : b` is **not** `c and a or b`, which yields `b` when `a` is `false` or `nil`.

`begin … end while c` (a do-while loop) is rejected.

### 4.5 Iteration
| Ruby | Becomes |
|---|---|
| `xs.each { \|x\| … }` | `for _, x in ipairs(xs) do … end` |
| `xs.each_with_index { \|x, i\| … }` | `ipairs`, with `i` counting from 0 |
| `m.each { \|k, v\| … }`, `m.each_pair { \|k, v\| … }` | `for k, v in pairs(m) do … end` |
| `n.times { \|i\| … }` | `for i = 0, n - 1 do … end` |
| `(a..b).each { \|i\| … }`, `a.upto(b) { \|i\| … }` | `for i = a, b do … end` |
| `(a...b).each { \|i\| … }` | `for i = a, b - 1 do … end` |
| `a.step(b, s) { \|i\| … }` | `for i = a, b, s do … end` |

`each` with **one** block parameter iterates an array and with **two** a map, since the
translator can't see types; the wrong one is SPEC.md's E0412, with a Ruby-worded hint. The
iteration rules of SPEC.md §3.3 apply: what is visited is fixed when the loop starts.

### 4.6 Literals
| Ruby | Becomes |
|---|---|
| `nil`, `true`, `false`, numbers | the same |
| `'text'`, `"text"` | a string |
| `"hp: #{hp}"` | `"hp: " .. tostring(hp)` |
| `:name` | the string `"name"` |
| `[a, b, c]` | an array |
| `{ hp: 10, name: "x" }` | a record |
| `{ "fire" => 2, "ice" => 3 }` | a map |
| `{}` | an empty table, shaped by its uses (SPEC.md §5.2) |

Interpolated values go through `tostring`, so each must be a `number`, `bool`, or `string`.
Record fields are read with `r.name` or `r[:name]` (the literal key `"name"`, SPEC.md §5.4).

### 4.7 Operators
| Ruby | Becomes |
|---|---|
| `+ - * / %` | the same; `/` is `f64` division and `%` is floored, as in both Ruby and Lua |
| `**` | `^` |
| `x.div(y)` | `x // y` |
| `== !=` | `== ~=` |
| `&&`, `and`; `\|\|`, `or`; `!`, `not` | `and`; `or`; `not` |
| `x += e` (and the other compound forms) | `x = x + e` |
| `x \|\|= d`, `x &&= e` | `x = x or d`, `x = x and e` |

`and`/`or` yield one of their operands as in SPEC.md §3.5, so `name || "unknown"` gives an
optional a default. `+` on strings is rejected: when an operand is a string literal the
translator reports it directly; otherwise the checker's type mismatch carries a hint to use
interpolation.

### 4.8 Nil tests and conditions
`x.nil?` becomes `x == nil`; `!x.nil?`, `x != nil`, and `unless x.nil?` become `x ~= nil`. The
checker narrows `x` in the then-branch of `x ~= nil` and, with C8, in the else-branch of
`x == nil` and the `elsif` arms after it, so Ruby's usual `if x.nil? … else … end` works as
written.

Conditions must be `bool` (SPEC.md §3.5). `if user` with `user` optional is an error whose
hint suggests `unless user.nil?`.

### 4.9 Arrays
Ruby indices **start at 0**. `a[i]` is an index node marked zero-based (C9): when `a` is an
array, lowering and the interpreter add 1 before the element access; when `a` is a map or a
record, the key is used as written. The translator can't make this choice itself, since `a[i]`
on an array and `m[k]` on a map look alike. Reads yield `T?` as in SPEC.md §5.4.

| Ruby | Becomes |
|---|---|
| `a[i]`, `a[i] = v` | zero-based element access (C9) |
| `a.first`, `a.last` | `a[1]`, `a[#a]` |
| `a.length`, `a.size` | `#a` |
| `a << x`, `a.push(x)` | `a[#a + 1] = x`, as a statement only |

A literal negative index (`a[-1]`) is an error; a computed negative index reads `nil`. An
out-of-range write is SPEC.md §7.3's runtime error and reports the element's 1-based position
_(open: report the Ruby index instead)_.

### 4.10 Host memory
`@name` is the field `name` of one memory binding the host designates for `@`
(`RubyConfig::ivar_memory`, for example `mem`); without one, `@name` is an error. Other
memory bindings are used by name, `mem.gold`. A call on a memory binding, `mem.add_item(id)`,
is a memory method call (SPEC.md §7.2; Lua's `mem:add_item(id)`). `@hp` in top-level code is
E0310, as in SPEC.md §3.1.

### 4.11 Methods on values
`recv.name` with no arguments is a **field read** unless `name` is in this table. A method in
the table always wins, so a record field named `size` is read as `r[:size]`.

| Ruby | Becomes |
|---|---|
| `x.length`, `x.size` | `#x` (string or array) |
| `x.nil?` | `x == nil` (§4.8) |
| `x.to_s` | `tostring(x)` |
| `s.to_f` | `tonumber(s)` (`number?`) |
| `x.abs`, `x.floor`, `x.ceil` | `math.abs(x)`, `math.floor(x)`, `math.ceil(x)` |
| `x.clamp(lo, hi)` | `math.min(math.max(x, lo), hi)` |
| `s.upcase`, `s.downcase` | `string.upper(s)`, `string.lower(s)` |
| `x.div(y)` | `x // y` |
| `a.first`, `a.last`, `a.push(x)`, `a << x` | §4.9 |
| `f.call(x)`, `f.(x)` | calling the closure `f` |
| `format(fmt, …)` | `string.format(fmt, …)` |
| `Math.sqrt(x)`, `Math::PI` _(open: `::`)_ | `math.sqrt(x)`, `math.pi` |

Any other method on a non-memory value is "not supported in Grindlang", naming the method.
The table only renames; SPEC.md §6's catalog is the single source of builtins.

### 4.12 Collection methods with blocks
| Ruby | Result |
|---|---|
| `xs.map { \|x\| e }` | a new array of each `e` |
| `xs.select { \|x\| c }`, `xs.reject { \|x\| c }` | a new array of the elements where `c` holds, or doesn't |
| `xs.sum`, `xs.sum { \|x\| e }` | a `number`, starting from 0 |
| `xs.count { \|x\| c }` | a `number` |
| `xs.any? { \|x\| c }`, `xs.all? { \|x\| c }` | a `bool` |
| `xs.find { \|x\| c }` | the first element where `c` holds, as `T?` |

Each becomes a loop, so it is accepted only as the **whole right side of an assignment** or the
value of a `return`. Elsewhere, such as an argument or an operand, it is an error asking for a
local. The block's last expression is its value. Blocks here and in §4.5 may use `next` and
`break`.

### 4.13 Lambdas and closures
`->(x) { x * 2 }`, `-> do … end`, and `lambda { |x| … }` are anonymous functions (SPEC.md
§5.5), with §4.2's return rule. They are called with `f.call(x)` or `f.(x)`; `f[x]` is not
a call. A named `def` is not a value (E0434); wrap it: `->(x) { helper(x) }`. Blocks exist only
on the iteration forms (§4.5, §4.12) and DSL keywords (§5); passing a block to your own `def`
(`yield`, `&block`) is not supported.

### 4.14 Multiple values
`return a, b` returns a tuple; `q, r = divmod(a, b)` binds one; `a, b = b, a` assigns in
parallel. SPEC.md §5.5's exact-arity rules apply.

## 5. DSL keywords

The host may declare **keywords** in `RubyConfig`. A keyword is written as a call without
parentheses at the start of a statement (§3.4). There are two kinds, both built by top-level
code, so what they build is frozen once the module has initialized.

### 5.1 Handler keywords
```ruby
MAX_HP = 100

on :hit do |dmg|
  @hp -= dmg
end

on :heal do |amount|
  @hp = (@hp + amount).clamp(0, MAX_HP)
end
```

The host declares the keyword's name, its block's signature (here `fn(number)`), the constant
that collects the blocks, and the name of a generated dispatch function. The script above
becomes:

```lua
MAX_HP = 100
ON = {}
ON["hit"] = function(dmg)
  mem.hp = mem.hp - dmg
end
ON["heal"] = function(amount)
  mem.hp = math.min(math.max(mem.hp + amount, 0), MAX_HP)
end

function on_event(id, arg)
  local h = ON[id]
  if h ~= nil then h(arg) end
end
```

- The first argument is the handler's id: a symbol or string literal. A repeated id is an
  error.
- Every block must fit the declared signature, since one table holds them all.
- A handler body may use host memory: it runs when called, after memory is bound. The table
  itself is frozen, so a handler can't replace another.
- The host calls `on_event("hit", 7.0)`, or reads the table with `Module::call("ON", vec![])`
  (SPEC.md §7.1) and invokes entries with `Module::call_value`.

### 5.2 Data keywords
```ruby
node :shop do
  say "What'll it be?"
  choice "Buy a potion", if: -> { @gold >= 5 } do
    @gold -= 5
  end
  choice "Leave"
end
```

The host declares a schema: the record each keyword builds, which arguments fill which fields
(positional arguments, `key:` arguments, and the block), which nested keywords append to which
array fields, and where top-level records go (here, a constant map `NODES` keyed by id). Every
field has a type and, unless required, a default, so every record of one keyword has the same
shape. With `if:` defaulting to `-> { true }` and the block to `-> {}`, the script becomes:

```lua
NODES = {}
NODES["shop"] = {
  lines = { "What'll it be?" },
  choices = {
    { text = "Buy a potion",
      cond = function() return mem.gold >= 5 end,
      effect = function() mem.gold = mem.gold - 5 end },
    { text = "Leave",
      cond = function() return true end,
      effect = function() end },
  },
}
```

Unknown keywords, unknown `key:` arguments, missing required fields, and repeated ids are
errors. The host reads `NODES` through `Module::call` and runs `cond` and `effect` with
`Module::call_value`.

### 5.3 Declaring keywords _(open: final API)_
A sketch of the embedding side; the names are illustrative:

```rust
let mut ruby = RubyConfig::new().ivar_memory("mem");
ruby.handler("on")
    .block(&[Type::Number], Type::Unit)
    .table("ON")
    .dispatch("on_event");
ruby.data("node")
    .keyed_into("NODES")
    .list("say", "lines", Type::String)
    .child("choice", "choices", |c| {
        c.positional("text", Type::String)
            .keyword("if", "cond", &[], Type::Bool, "-> { true }")
            .block("effect", &[], Type::Unit, "-> {}")
    });
let module = engine.compile_ruby(src, &ruby)?;
```

## 6. Rejected constructs

Each produces a targeted, span-pointing diagnostic. Ruby-only diagnostics use **E05xx**
(lexing and parsing) and **E06xx** (translation and DSL keywords); numbers are assigned as
they are implemented. SPEC.md §8's diagnostics apply unchanged, worded in Ruby syntax where
they quote code.

- **Object model:** `class`, `module`, `self`, `super`, `Struct`, `.new`, `attr_*`,
  `define_method`, `method_missing`, `send`, `public_send`, `instance_variable_get`, `eval`,
  `instance_eval`.
- **Blocks and procs:** `yield`, `&block` parameters, `block_given?`, a block passed to a
  user `def`, `proc`, `Proc.new`, `f[x]` as a call.
- **Exceptions:** `begin`/`rescue`/`ensure`/`retry`, `raise`, the `rescue` modifier.
- **Parameters:** default values, `*args`, `**opts`, keyword parameters.
- **Variables:** `$globals`, `@@class_vars`, `@ivar` with no memory designated for `@`, and a
  `def` reading a top-level lowercase local.
- **Literals:** heredocs, `%w`/`%i`/`%q`/`%Q`, `?a`, regex literals, backticks, `0b`/`0o`,
  rationals, imaginaries, quoted symbols, literal negative indices.
- **Syntax:** `for … in`, `=begin`/`=end`, `__END__`, `BEGIN`/`END`, `alias`, `undef`,
  `defined?`, `&.`, `<=>`, `===`, `=~`, bitwise operators, `begin … end while`, a call without
  parentheses anywhere but the start of a statement, or with a first argument beginning with
  `-`, `[`, `(`, `*`, `&`, or `{`.
- **Semantics:** `+` with a string-literal operand; a collection method (§4.12) outside an
  assignment or `return`; an `if` or `case` value with multi-statement branches outside an
  assignment or `return`.

## 7. Worked example

A stat module in the Ruby surface, and what it becomes:

```ruby
XP_BASE = 10

private def curve(level)
  level * level
end

def xp_for(level)
  curve(level) * XP_BASE
end

# @param items {name: string, score: number}[]
def best_name(items)
  best = nil
  items.each do |item|
    if best.nil?
      best = item
    elsif item[:score] > best[:score]
      best = item
    end
  end
  best.nil? ? "none" : best[:name]
end
```

```lua
XP_BASE = 10

function curve(level)
  return level * level
end

function xp_for(level)
  return curve(level) * XP_BASE
end

---@param items {name: string, score: number}[]
function best_name(items)
  local best = nil
  for _, item in ipairs(items) do
    if best == nil then
      best = item
    elseif item["score"] > best["score"] then   -- best narrowed here (C8)
      best = item
    end
  end
  return (best == nil) ? "none" : best["name"]  -- a conditional expression (C2);
end                                             -- no Lua spelling

return { XP_BASE = XP_BASE, xp_for = xp_for, best_name = best_name }
```

`curve` stays callable from the whole file and is left out of the export table.

## Open items

- **Method names ending in `?` or `!`.** Idiomatic Ruby (`def alive?`), but the exported name
  must be one the host can call and the backends can carry. v1 rejects them; allowing `?` needs
  a check that names flow unchanged through lowering and codegen.
- **Index errors.** An out-of-range write reports the 1-based position. Reporting the Ruby
  index needs the zero-based flag (C9) carried into the runtime error.
- **`::` for `Math::PI`.** Accept `Math::PI` and `Math.sqrt` as spellings of `math.*`, or only
  the latter.
- **The keyword declaration API** (§5.3), settled in PLAN.md Phase 10, step R5.
