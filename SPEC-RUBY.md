# Grindlang — Ruby Surface Specification

> Companion to [SPEC.md](SPEC.md) and [PLAN.md](PLAN.md) Phase 10. SPEC.md defines what a
> Grindlang program **means**; this document defines only the **Ruby spelling** of it: which
> Ruby forms are accepted and which SPEC.md construct each one becomes. Where this document is
> silent, SPEC.md governs.
>
> Status: **implemented**, behind the `ruby` feature: the lexer and parser (§2, §3, and the
> syntax rejections of §6; `grindlang::ruby::parse`), the translation of §4 and the DSL keywords
> of §5 into the shared tree (`grindlang::ruby::parse_module`), `Engine::compile_ruby`, and the
> CLI (§1). The decisions below were confirmed with the stakeholder on 2026-09-30; details
> marked _(open)_ are still to settle, and are listed under Open items. Items that depend on a
> core change name it (C2–C10, PLAN.md Phase 10).

## 1. What a Ruby-surface script is

A `.rb` Grindlang script is a module definition with exactly the model of SPEC.md §1 and §4:
it evaluates to a table of exported functions and constants, its top-level code runs **once**
and everything it builds is then **frozen**, and only host memory survives between calls.

The Ruby surface is a **strict subset of Ruby's syntax**, not a Ruby implementation. There are
no objects, classes, or exceptions; types are static and inferred (SPEC.md §5); numbers are
`f64`; conditions are `bool`. A construct outside this document is rejected with a
"not supported in Grindlang" diagnostic that names it — never a generic parse error.

The host picks the surface: `Engine::compile_ruby(src, &RubyConfig)` in the embedding API
(§4.1, §5.4), and in the `grindlang` CLI the `.rb` extension, overridden by
`--surface lua|ruby`. The CLI's IR cache key includes the surface. The CLI declares no DSL
keywords and designates no memory for `@name`, since it binds no memory at all.
`grindlang --print-lua script.rb` prints what a script translates into, as Lua, without
checking or running it (`grindlang::unparse::to_lua`). Lua can't write everything a translation
holds — a conditional expression, an index counting from 0, a name the translator made up — so
those print in a form a reader can follow, with a comment at the top saying how.

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
- Reserved at the top level: `math`, `string`, `tostring`, and `tonumber`, the builtins the
  translation calls (`x.abs` is `math.abs(x)`), so a `def` or top-level local can't take one of
  those names (SPEC.md's `E0304`).

### 2.4 Keywords
```
and    break  case   def    do     else   elsif  end    false  if
loop   next   nil    not    or     private public return then  true
unless until  when   while
```
`loop`, `private`, and `public` are methods in Ruby; here they are fixed forms. After a `.`, a
keyword is an ordinary method name (`x.then`), and directly followed by `:` where a hash key or
named argument can start, a label (`if: cond`).
Reserved and rejected wherever they appear: `class`, `module`, `self`, `super`, `yield`,
`begin`, `rescue`, `ensure`, `retry`, `redo`, `raise`, `alias`, `undef`, `defined?`, `for`
(use `.each`), `in`, `BEGIN`, `END`, `__method__`, `__FILE__`, `__LINE__`, `__ENCODING__`,
`__END__`.

### 2.5 Numbers
All numeric literals are `f64`: `3`, `3.0`, `0.5`, `1e3`, `2.5e-4`, `0xFF`, with `_` allowed
between digits (`1_000`). As in Ruby, a `.` belongs to a number only when a digit follows it, so
`3.times` is a method call, and there is no `.5` or `3.`. A `-` written directly before a number
makes a negative literal, so `-2.abs` is `(-2).abs`, except that `-2 ** 2` is `-(2 ** 2)`.
Rejected: `0b`, `0o`, and `0d` literals, octal `017`, rationals (`3r`), imaginaries (`2i`).

### 2.6 Strings
- `'…'`: raw, with only `\\` and `\'` escapes, as in Ruby.
- `"…"`: Ruby's escapes `\n \t \r \s \e \a \b \f \v \\ \" \' \#`, octal `\0`–`\377`, `\xNN`,
  `\uNNNN`, and `\u{N…}`, and a `\` at the end of a line joining it to the next; any other
  escape is an error. Interpolation `#{expr}` (§4.6); Ruby's shorthand `"#@hp"` is rejected,
  since it would silently read as text.
- A string may span lines.
- Rejected: heredocs, `%w[]`, `%i[]`, `%q`, `%Q`, `?a` character literals, backticks, regex
  literals.

A `/`, `%`, `?`, `<<`, or `:` where an operand is expected would start a regex, a `%` literal, a
character literal, a heredoc, or a symbol. After a name, Ruby also reads a `/`, `%`, or `<<`
with a space before it and none after (`say /hi/`, `xs <<x`) as the start of a literal
argument; that is rejected, with a hint to space the operator on both sides.

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
topitem   ::= ['private' | 'public'] def
            | 'private' | 'public'          -- switch the visibility of later defs
            | stat
def       ::= 'def' Name ['(' [params] ')'] body 'end'
params    ::= Name {',' Name}
body      ::= {stat sep}
```
A `def`, `private`, or `public` anywhere but the top level is an error; a local function is a
lambda assigned to a local. Without parentheses, a `def`'s name must end its line.

### 3.2 Statements
```ebnf
stat ::= expr [modifier]
       | assign [modifier]
       | command [modifier]
       | 'if' expr ['then'] body {'elsif' expr ['then'] body} ['else' body] 'end'
       | 'unless' expr ['then'] body ['else' body] 'end'
       | 'case' expr {'when' arg {',' arg} ['then'] body} ['else' body] 'end'
       | 'while' expr ['do'] body 'end'
       | 'until' expr ['do'] body 'end'
       | 'loop' block
       | 'break' | 'next'
       | 'return' [arg {',' arg}]

modifier ::= ('if' | 'unless' | 'while' | 'until') expr
assign   ::= target {',' target} '=' arg {',' arg}
           | target opassign arg
target   ::= Name | Const | Ivar | primary '[' arg ']' | primary '.' Name
opassign ::= '+=' | '-=' | '*=' | '/=' | '%=' | '**=' | '||=' | '&&='
```
`break` and `next` take no value. An assignment is a statement, never a value: chained
assignment (`a = b = 0`) and assignment inside a condition are errors.

### 3.3 Expressions
As in Ruby's own grammar, expressions come in two levels. An `expr` may use `not`, `and`, and
`or`; it is what conditions, modifiers, expression statements, parentheses, and interpolations
hold. Everywhere else — the right side of `=`, `return` values, arguments, elements, indices,
and ternary branches — takes an `arg`, which may not. Ruby reads `x = a or b` as
`(x = a) or b`, so it is rejected rather than read either way; write `x = a || b` or
`x = (a or b)`.

```ebnf
expr     ::= expr ('and' | 'or') expr | 'not' expr | arg
arg      ::= arg binop arg | unop arg
           | arg '?' arg ':' arg
           | arg ('..' | '...') arg              -- a range (§4.5)
           | primary

primary  ::= 'nil' | 'true' | 'false' | Number | String | Symbol
           | Name | Const | Ivar
           | '(' expr ')'
           | array | hash | lambda
           | call
           | primary '[' arg ']'
           | primary '.' Name                     -- field read or method (§4.11)
           | 'if' … 'end' | 'unless' … 'end' | 'case' … 'end'   -- as values (§4.4)

call     ::= Name '(' [args] ')' [block]
           | Name block
           | primary '.' Name '(' [args] ')' [block]
           | primary '.' Name block
           | primary '.' '(' [args] ')'           -- calling a closure: f.(x)
args     ::= callarg {',' callarg}
callarg  ::= arg | Name ':' arg                   -- `key: value` only for DSL keywords (§5)
block    ::= 'do' ['|' params '|'] body 'end'
           | '{' ['|' params '|'] body '}'
lambda   ::= '->' ['(' [params] ')'] ('{' body '}' | 'do' body 'end')
           | 'lambda' block
array    ::= '[' [arg {',' arg} [',']] ']'
hash     ::= '{' [pair {',' pair} [',']] '}'
pair     ::= Name ':' arg | arg '=>' arg
```
A range is accepted only as an iteration receiver, `(a..b).each`, and as a `when` value; the
translator rejects it elsewhere. Line breaks may appear inside `( )`, `[ ]`, and `{ }` after
an element and before the closing bracket.

### 3.4 Calls without parentheses
```ebnf
command  ::= Name callarg {',' callarg} [block]
```
A call may omit its parentheses **only at the start of a statement** (`say "Hi"`,
`on :hit do … end`) and only when it starts with a plain name: `mem.add_item(:sword)` needs
them. Its first argument may not begin with `-`, `[`, `(`, `*`, `&`, or `{`: after a name
those are an operator, an index, the call's own parentheses, or its block, so `foo [1]`
indexes and `foo { … }` passes a block. A hash argument therefore needs parentheses,
`foo({ a: 1 })`. Where Ruby would decide by spacing — `foo -1`, `foo *xs`, `foo [1]`, and
`foo (1)`, with a space before the operator and none after — the parser rejects the form and
asks for `foo(-1)` or `foo - 1`. A name followed by an argument anywhere else (`x = foo 1`) is
an error too. A bare name with no arguments (`reset`) is a zero-argument call unless a local
of that name is in scope (§4.3).

A `do` block belongs to a statement's call without parentheses when there is one
(`on :hit do … end`, `say xs.first do … end` gives the block to `say`), and a `while` or
`until` condition never takes one (`while xs.any? do` is the loop's `do`). Otherwise a block
belongs to the call just before it.

### 3.5 Precedence (lowest → highest)
```
modifiers                 -- x if c, x unless c, x while c, x until c
and  or                   -- equal precedence, left-associative, as in Ruby
not                       -- keyword form
=  op=                    -- a statement (§3.2), never inside an expression
?  :                      -- right-associative
..  ...
||
&&
==  !=
<  <=  >  >=
<<                        -- array append
+  -
*  /  %
-                         -- unary minus
**                        -- right-associative; binds tighter than unary minus: -x ** 2 == -(x ** 2)
!                         -- unary
```
The translator builds the tree by these rules, so Ruby's `a or b and c` is `(a or b) and c`
even though SPEC.md's Lua spelling ranks `and` above `or`, and `not a and b` is
`(not a) and b`. Postfix forms — `.name`, calls, `[i]` — bind tighter than any operator:
`-x.abs` is `-(x.abs)`, and `!x.nil?` is `!(x.nil?)`.

## 4. How Ruby maps onto Grindlang

### 4.1 Top level
| Ruby | Becomes |
|---|---|
| `MAX_HP = 100` | the constant `MAX_HP`, exported (SPEC.md §3.1) |
| `names = []` (lowercase) | a top-level `local`: a private module variable |
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

A constant is assigned only directly at the top level, once, with one value: `X = 1` in a
`def`, a block, or a branch, `A, B = 1, 2`, and `X = 1, 2` are `E0604`.

The host compiles a script with `Engine::compile_ruby(src, &RubyConfig)`, which translates it
against the engine's host functions and memory and compiles the result as `Engine::compile`
does a Lua script. Underneath, `ruby::parse_module(src, &RubyConfig, &TypeConfig)` returns the
shared `ast::Module` that `analyze_module` and `compile_module` take with the same
`TypeConfig`: the translator reads the memory bindings and memory methods from it (§4.10).

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
`number?`. A bare `return` does the same, and so does returning a call to a method that returns
nothing: `def f(c) if c then 1 else log end end` is `number?`, and `nil` where it logs. A `def`
none of whose paths gives a value — `def f(c) log if c end` — returns nothing (SPEC.md §5.5).
A declared `# @return number` keeps its type, so reaching the end is then an error (`E0480`),
as is reaching it in a `def` that returns several values (`return a, b`) on another path.
Explicit `return` otherwise works as in SPEC.md.

A DSL keyword's block returns as its declared signature says instead (§5.3): a block declared
to return nothing never gets an implicit `return`.

### 4.3 Local variables
A Ruby local is declared by its first assignment, as Ruby's parser sees it: a name is a local
from the first assignment to it on, and before that, the same name is a call with no
arguments. The translator finds each variable's owning scope — a `def`, a block or lambda, or
the top level — and:
- emits `local x = …` at the first assignment when every other use of `x` is inside the block
  that assignment lands in (a branch, a loop body, or the block a modifier's statement forms);
- otherwise emits a bare `local x` at the start of the owning scope. It holds `nil` until
  assigned and is typed as optional (SPEC.md §5.2), matching Ruby, where a variable assigned in
  only one branch is `nil` on the others. So is a variable whose first assignment reads it
  (`x ||= 1`, `x += 1`, a lambda that calls itself), and every new variable of a multiple
  assignment that also assigns something else (`a, b = 1, 2` where `a` exists).

A block sees its enclosing scope's locals; a variable first assigned inside a block is local to
that block. Block parameters shadow outer names.

One difference from Ruby: a variable first assigned directly in a `while` or `until` body is
declared there, so it is a new variable on each pass, where Ruby has one for the whole method.
Only a lambda made in the loop and called on a later pass can tell.

### 4.4 Control flow
| Ruby | Becomes |
|---|---|
| `if`/`elsif`/`else`, `unless`/`else` | `if`/`elseif`/`else`, with `unless c` as `if not c` |
| `stmt if c`, `stmt unless c` | a one-branch `if` |
| `case x when a, b … when c … else … end` | an `if` chain testing `x == a or x == b`, …; a local or constant `x` is read again for each `when`, and any other is evaluated once, into a temporary |
| `when lo..hi`, `when lo...hi` | `x >= lo and x <= hi`, `x >= lo and x < hi` |
| `while c`, `until c`, `stmt while c` | `while c`, `while not c` |
| `loop do … end` | `while true do … end` |
| `break`, `next` | `break`, `continue` (C3) |
| `c ? a : b`; `if`/`unless`/`case` used as a value | a conditional expression (C2) |

`next` goes on to the next pass of the innermost loop, a block of §4.5 included; outside a loop
it is SPEC.md's `E0311`, and directly in a block of §4.12, `E0611`.

What an `if`, `unless`, `case`, or `?:` becomes depends on where it is:

- **At the end of a body** (§4.2) and **as the value of a `return`**, it is an `if` statement
  whose branches each return their value, so a branch may hold statements, and one that ends in
  a statement returns nothing, as §4.2 says. `c ? a : b` there is
  `if c then return a else return b end`. After a `return` of one, a path through a branch
  that gives no value returns nothing. **As any other statement**, each branch runs as one:
  `c ? attack : defend` calls one of the two.
- **As the whole right side of an assignment**, it is a conditional expression (C2) when each
  branch is one expression: `x = c ? a : b` is `local x = c ? a : b` in the shared tree, typed
  like `a or b` (SPEC.md §3.5). When a branch holds statements, the assignment moves into each
  branch instead, `if c then …; x = e1 else …; x = e2 end`, and a local it declares is declared
  just before with no value, so it is optional, like the locals of §4.3; each branch must then
  end in an expression, a jump, or `stmt if c` (`E0609`). A `case` here whose subject is not a
  local or constant evaluates it once, into a temporary declared just before the assignment.
- **Anywhere else** — an operand, an argument, a condition, one of several values — it is a
  conditional expression, so each branch must be one expression, and a `case` subject a local
  or constant it can read again for each `when` (`E0609`).

A missing `else`, or an empty branch, gives `nil`, as in Ruby: `x = if c then 1 end` is a
`number?`. The condition narrows the branches as an `if` does, so `x.nil? ? 0 : x + 1` needs no
more. `c ? a : b` is **not** `c and a or b`, which yields `b` when `a` is `false` or `nil`. A
constant's value is one expression (§4.1), so each branch of an `if` or `case` giving it is one
too.

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
translator can't see types; the wrong one is SPEC.md's E0412, worded for Ruby ("`each` with
one block parameter (or `map`, `select`, `sum`, …) expects an array"). The iteration rules of
SPEC.md §3.3 apply: what is visited is fixed when the loop starts. `times`, `upto`, `step`,
and a range's `each` take at most one block parameter, `each_pair` exactly two; a block may
leave out the ones it doesn't use. Each form is a loop, so it is a statement: as a value it is
`E0602`. Its receiver may be a collection method (§4.12): `xs.select { … }.each { … }`.

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
| `{}` | rejected (`E0608`, _open_): SPEC.md has no empty map, since `{}` is an empty array |

Interpolated values go through `tostring`, so each must be a `number`, `bool`, or `string`,
and interpolating a parameter doesn't settle its type: annotate it (`# @param name string`).
Record fields are read and assigned with `r.name` or `r[:name]` (the literal key `"name"`,
SPEC.md §5.4).

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
translator reports it directly (`E0600`); otherwise the checker's type mismatch carries a hint
to use interpolation.

A compound assignment reads its target and writes it, so a target holding a call
(`g[0] += 1`, where `g` is a method) is `E0602`, which asks for a local: the call would run
twice.

### 4.8 Nil tests and conditions
`x.nil?` becomes `x == nil`; `!x.nil?`, `x != nil`, and `unless x.nil?` become `x ~= nil`. The
checker narrows `x` in the then-branch of `x ~= nil` and, with C8, in the else-branch of
`x == nil` and the `elsif` arms after it, so Ruby's usual `if x.nil? … else … end` works as
written.

Conditions must be `bool` (SPEC.md §3.5). `if user` with `user` optional is an error whose
hint suggests `unless user.nil?`. `unless c` is `if not c`, except that the negation of `==`
or `!=` is the other comparison, so `unless x.nil?` narrows as `x ~= nil` does. Narrowing
follows SPEC.md §5.3, so a guard clause, `return 0 if x.nil?`, narrows nothing after it
_(open)_.

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
_(open: report the Ruby index instead)_. `a.last` and `a << x` read `a` twice, so an `a`
holding a call is `E0602`.

### 4.10 Host memory
`@name` is the field `name` of one memory binding the host designates for `@`
(`RubyConfig::ivar_memory`, for example `mem`); without one, or when the host provides no
memory of that name, `@name` is `E0524`. Other memory bindings are used by name, `mem.gold`.
A call on a memory binding, `mem.add_item(id)`, is a memory method call (SPEC.md §7.2; Lua's
`mem:add_item(id)`), and so is `mem.reset` with no parentheses when the host declares a method
`reset`; otherwise `mem.name` is a field. A call of a method the host doesn't declare is
`E0601`. A local of the same name hides the memory binding. `@hp` in top-level code is E0310,
as in SPEC.md §3.1.

### 4.11 Methods on values
`recv.name` with no arguments is a **field read** unless `name` is in this table. A method in
the table always wins, so a record field named `size` is read as `r[:size]`.

| Ruby | Becomes |
|---|---|
| `x.length`, `x.size` | `#x` (string or array) |
| `x.nil?` | `x == nil` (§4.8) |
| `x.empty?`, `x.zero?` | `#x == 0`, `x == 0` |
| `x.to_s` | `tostring(x)` |
| `s.to_f` | `tonumber(s)` (`number?`) |
| `x.abs`, `x.floor`, `x.ceil` | `math.abs(x)`, `math.floor(x)`, `math.ceil(x)` |
| `x.clamp(lo, hi)` | `math.min(math.max(x, lo), hi)` |
| `s.upcase`, `s.downcase` | `string.upper(s)`, `string.lower(s)` |
| `x.div(y)` | `x // y` |
| `a.first`, `a.last`, `a.push(x)`, `a << x` | §4.9 |
| `f.call(x)`, `f.(x)` | calling the closure `f` |
| `x.freeze` | `x` (§4.1) |
| `format(fmt, …)` | `string.format(fmt, …)`, unless the script or host defines `format` |
| `Math.sqrt(x)`, and any other function of `math` | `math.sqrt(x)` |
| `Math.pi`, `Math.huge`; `Math::PI` _(open: `::`)_ | `math.pi`, `math.huge` |

A method in the table checks its argument count (`E0602`) and takes no block (`E0521`). Any
other method called with parentheses or arguments, or whose name ends in `?` or `!`, is
`E0601`, "not supported in Grindlang", naming the method. The table only renames; SPEC.md
§6's catalog is the single source of builtins.

### 4.12 Collection methods with blocks
| Ruby | Result |
|---|---|
| `xs.map { \|x\| e }` | a new array of each `e` |
| `xs.select { \|x\| c }`, `xs.reject { \|x\| c }` | a new array of the elements where `c` holds, or doesn't |
| `xs.sum`, `xs.sum { \|x\| e }` | a `number`: 0 plus each element, or each `e`, in order |
| `xs.count { \|x\| c }` | a `number` |
| `xs.any? { \|x\| c }`, `xs.all? { \|x\| c }` | a `bool`, stopping at the first element that settles it |
| `xs.find { \|x\| c }` | the first element where `c` holds, as `T?` |

The receiver is an array, or a range, `(a..b)` or `(a...b)`, counted through as in §4.5:
`(1..n).sum { |i| i * i }`. The block takes at most one parameter, the element, so a hash is
SPEC.md's `E0412`, as for `each` with one. Only `sum` may leave out the block, and none takes
arguments (`E0602`; `xs.count` alone suggests `length`).

Each becomes a loop that builds its result in a temporary, and the call is that temporary.
`ys = xs.select { |x| x > 1 }` is:

```lua
local t = {}
for _, x in ipairs(xs) do
  if x > 1 then t[#t + 1] = x end
end
local ys = t
```

The loop runs before the statement holding the call, so a call is accepted only where its
value is the first thing that statement evaluates, which then can't tell the difference: the
**whole right side of an assignment** (not `+=`), the **value of a `return`** or of the end of a
body, a **block's value**, so that calls nest (`xss.map { |xs| xs.sum }`), and the receiver of
a loop of §4.5 or the subject of a `case`; and at the **start of a method chain** in one of
those: `xs.select { … }.map { … }.sum`, `xs.select { … }.length`. Anywhere else — an operand,
an argument, a condition — running the loop first could change what the program does, so it
is `E0610`, which asks for a local. In an `if` or `case` giving an assignment its value, a
branch that is such a call counts as holding statements (§4.4).

The block's last expression is its value for each element: an `if`, `case`, or `?:` there is a
conditional expression (§4.4), and the statements before it run first. A `return` in the block
returns from the enclosing `def` or lambda, as in Ruby. Ruby's `next` would give the block
`nil`, and its `break` would make the whole call `nil`, neither of which has a typed
equivalent here, so either one directly in the block is `E0611`, as is a block that is empty
or ends in a statement with no value. In a loop inside the block, they work as usual.

`select`, `reject`, and `find` give the elements themselves, even when the block assigns its
parameter. When it doesn't, their test narrows the element as an `if`'s does, so
`xs.reject { |x| x.nil? }` is an array of `T` for an array of `T?`. `sum` adds in order, so a
sum of fractions can differ from Ruby's, which compensates for rounding, in the last digits.

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

The host may declare **keywords** on `RubyConfig` (§5.4). There are two kinds: a **handler**
keyword gathers blocks into a table of closures, and a **data** keyword builds a table of
records. A keyword is written as a call without parentheses (§3.4), as a statement of its own
directly at the top level, and a data keyword's **nested** keywords directly in its block.
Anywhere else — in a `def`, a block, a branch, or a loop, or under a modifier — a keyword is
`E0612`, as is a `def` or constant taking the name of a keyword, its table, or its dispatch
function. A nested keyword's name means nothing special outside its parent's block, so a `def`
or host function may have it.

Each use is ordinary top-level code that runs where it is written. The first use of a keyword
declares its **table**, an exported constant mapping each id to an entry, and each later use
adds to it, so what the keywords build is frozen once the module has initialized, and a table
read before its first use has run is a runtime error, as for any constant (SPEC.md §3.1). A
script that never uses a keyword has no table for it, since SPEC.md has no empty map (§4.6).

Every top-level use starts with its **id**: a symbol or a string written out, `:hit` or
`"hit"` (`E0614` otherwise, `E0615` without one). An id names one entry of its keyword
(`E0616`), and `:hit` and `"hit"` are the same id.

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

The host declares the keyword's name, its table, the signature every block must fit (here
`fn(number)`), and optionally a **dispatch function** (here `on_event`). The script above
becomes, in effect:

```lua
MAX_HP = 100
ON = { ["hit"] = function(dmg) mem.hp = mem.hp - dmg end }
ON["heal"] = function(amount)
  mem.hp = math.min(math.max(mem.hp + amount, 0), MAX_HP)
end

function on_event(id, arg)
  local h = ON[id]
  if h ~= nil then h(arg) end
end
```

- A use takes its id and a block, nothing else: a `key:` argument is `E0613`, another argument
  `E0614`, and a use without a block `E0615`. The block is a function of the declared
  signature (§5.3).
- The dispatch function takes the id and the block's arguments, and runs the handler for that
  id, giving what it gives; for an id with no handler it does nothing and gives `nil`, so it
  returns the block's type made optional. With no handler at all, it does nothing.
- A handler may use host memory: it runs when called, after memory is bound. The table is
  frozen, so a handler can't replace another.
- The host calls `on_event("hit", 7.0)`, or reads the table with `Module::call("ON", vec![])`
  (SPEC.md §7.1) and invokes an entry with `Module::call_value`. Scripts may call the
  dispatch function too.

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

The host declares the record each keyword builds, field by field: a field is filled by a
positional argument, a named argument (`if:`), the keyword's block, or the nested keywords in
its block, each of which adds to an array. Every field has a type, and every record of one
keyword has every field. With `choice`'s `if:` and block optional, the script becomes, in
effect:

```lua
NODES = {
  ["shop"] = {
    lines = { "What'll it be?" },
    choices = {
      { text = "Buy a potion",
        cond = function() return mem.gold >= 5 end,
        effect = function() mem.gold = mem.gold - 5 end },
      { text = "Leave", cond = nil, effect = nil },
    },
  },
}
```

- Positional arguments fill the positional fields in order, after the id at the top level;
  more than there are fields is `E0614`. A named argument fills its field: one the keyword
  doesn't have is `E0613`, and one given twice `E0614`.
- The block either fills a field, as a function (§5.3), or holds nested keywords and nothing
  else (`E0613`), taking no parameters (`E0614`). A **list** keyword (`say`) takes one value,
  and a **nested data** keyword (`choice`) builds a record as a top-level one does, without an
  id. Each adds to its array in the order written.
- A field a use leaves out takes its default: `nil` for an optional one, the literal the host
  gave, or an empty array for nested keywords. Leaving out any other is `E0615`.
- The arguments are top-level code, evaluated when the use runs, so they can't use host
  memory (`E0310`); blocks and lambdas are functions, which may, when the host calls them. A
  use's arguments are evaluated in the order the host declared its fields, not the order they
  are written, which only a host function with side effects could tell.

The host reads `NODES` through `Module::call` and runs `cond` and `effect` with
`Module::call_value`.

### 5.3 Blocks and types
A keyword's block, and a lambda written as an argument for a field of function type, become
functions of the declared type:

- It may leave out parameters at the end that it doesn't use; more than the type has is
  `E0614`.
- Its last expression gives its value only if the type gives one (§4.2): a block that gives
  nothing never gets an implicit return, so `on :hit do |d| log(d) end` doesn't return what
  `log` does.
- Its parameters and return type are the declared ones before its body is checked (SPEC.md
  §5.5, C10), so a mistake inside is reported where it is: `@name = d` for a `number` `d` is
  an assignment mismatch at `@name`.

Every argument is checked against its field's declared type where it is written. To do it, the
translator declares one private function per keyword, before any other code, whose parameters
are annotated with the declared types, and each use calls it: a record's builds the record,
and a handler's or list keyword's gives back its argument. They and the dispatch functions
have names no Ruby name can be (`choice#3`).

### 5.4 Declaring keywords
```rust
let mut ruby = RubyConfig::new().ivar_memory("mem");
ruby.handler("on", "ON")
    .block(&[Type::Number], Type::Unit)
    .dispatch("on_event");
ruby.data("node", "NODES")
    .list("say", "lines", Type::String)
    .child("choice", "choices", |c| {
        c.positional("text", Type::String)
            .keyword("if", "cond", Type::function(vec![], Type::Bool))
            .optional()
            .block("effect", &[], Type::Unit)
            .optional()
    });
let mut game = engine.compile_ruby(src, &ruby)?;
```

- `handler(keyword, table)` declares a handler keyword. `block(params, ret)` sets the
  signature its blocks fit, `ret` `Type::Unit` for none, and without it a block takes and gives
  nothing; `dispatch(name)` adds the dispatch function.
- `data(keyword, table)` declares a data keyword and returns its `Schema`, on which fields are
  declared in order: `positional(field, ty)`; `keyword(label, field, ty)`, whose label may be a
  Ruby keyword (`if:`); `block(field, params, ret)`; `list(keyword, field, ty)`, an array of
  `ty`; and `child(keyword, field, |schema| …)`, an array of records. `optional()` lets a use
  leave out the field declared last, which is then `nil`, making its type optional, and
  `default(value)` gives it a literal default: `nil`, a bool, a number, or a string of its type.
  A field whose type is optional is optional already.
- The declarations panic on what no script could use: a name that isn't one; a keyword name
  that is a Ruby keyword or that the translator gives a meaning (`lambda`, `format`, `math`, …);
  a keyword, table, or dispatch name another keyword already takes; a type a script can't write
  (a tuple, host userdata, or `()` other than as what a function gives); a block that fills two
  fields, or a field and nested keywords; or a default that doesn't fit.
- `Engine::compile_ruby` translates with the `RubyConfig` against the engine's own
  `TypeConfig`, which `Engine::type_config` returns for a host translating with
  `ruby::parse_module` itself.

## 6. Rejected constructs

Each produces a targeted, span-pointing diagnostic. Ruby-only diagnostics use **E05xx**
(lexing and parsing) and **E06xx** (translation and DSL keywords). SPEC.md §8's diagnostics
apply unchanged, worded in Ruby syntax where they quote code; a tree nested more than 200
levels deep is SPEC.md's `E0209`, with an `if` or `case` counting once as an expression and
once for its branches.

- **Object model** (`E0520`): `class`, `module`, `self`, `super`, `Struct`, `.new`, `attr_*`,
  `define_method`, `method_missing`, `send`, `public_send`, `instance_variable_get`, `eval`,
  `instance_eval`.
- **Blocks and procs** (`E0521`): `yield`, `&block` parameters, passing a block with `&`,
  `block_given?`, `proc`, `Proc.new`, block-local variables `|x; y|`. _Translation:_ a block
  passed to anything but a loop method or DSL keyword, including a user `def`, or to a method of
  §4.11. (`f[x]` meant as a call is the checker's `E0424`: a function can't be indexed.)
- **Exceptions** (`E0522`): `begin`/`rescue`/`ensure`/`retry`, `raise`, the `rescue` modifier,
  and with `begin`, `begin … end while`.
- **Parameters** (`E0523`): default values, `*args`, `**opts`, keyword parameters,
  destructuring `(a, b)`, splat arguments. _Translation:_ named arguments (`f(a: 1)`) outside a
  DSL keyword.
- **Variables** (`E0524`): `$globals`, `@@class_vars`. _Translation:_ `@ivar` with no memory
  designated for `@`, or designated but not provided, and a `def` reading a top-level lowercase
  local.
- **Literals** (`E0525`): heredocs, `%w`/`%i`/`%q`/`%Q`, `?a`, regex literals, backticks,
  `0b`/`0o`/`017`, rationals, imaginaries, quoted symbols and `"key":` keys, `"#@x"`, literal
  negative indices.
- **Syntax** (`E0526`): `for … in`, `case … in`, `=begin`/`=end`, `__END__`, `BEGIN`/`END`,
  `alias`, `undef`, `redo`, `defined?`, `&.`, `::`, `<=>`, `===`, `=~`, `!~`, bitwise operators
  and their assignments.
- **Calls without parentheses** (`E0527`): anywhere but the start of a statement, and the
  spacing-dependent forms of §3.4 (`foo -1`, `foo *xs`, `foo [1]`, `foo (1)`).
- **Loose `and`/`or`** (`E0528`): after an assignment or a call without parentheses, which
  Ruby would apply to the whole statement.
- **Statement structure:** a stray `end` at the top level (`E0513`); a `def`, `private`, or
  `public` below the top level, or `private` with arguments (`E0515`); a method name ending in
  `?` or `!` (`E0516`, see Open items); `break` or `next` with a value (`E0517`); chained
  assignment, assignment as a value, or an unassignable target (`E0512`).
- **Everything else** is a plain syntax error: `E0500`–`E0503` from the lexer (a stray
  character, an unterminated string, a bad escape, a malformed number), `E0510` for an
  unexpected token, and `E0511` where an expression was expected.
- **Semantics** (E06xx, translation):
  - `E0600`: `+` with a string-literal operand (§4.7).
  - `E0601`: a method that isn't in §4.11's table, a memory method the host doesn't declare,
    or a `Math` function the catalog doesn't have.
  - `E0602`: a method used the wrong way: the wrong number of arguments, a loop method without
    a block or used as a value, `push` or `<<` used as a value, and `.last`, `<<`, or a
    compound assignment reading a receiver that holds a call twice; a collection method (§4.12)
    given arguments, a block of more than one parameter, or no block but `sum`.
  - `E0603`: a range anywhere but a loop's receiver (§4.5, §4.12) or a `when` value.
  - `E0604`: a constant assigned anywhere but directly at the top level, or with more than one
    value.
  - `E0605`: a value used as a statement, except the last one of a `def` or lambda.
  - `E0606`: a statement after `return`, `break`, or `next` in the same body.
  - `E0607`: a local called like a method, `f(x)`, rather than as a lambda, `f.(x)`.
  - `E0608`: an empty hash (Open items).
  - `E0609`: an `if` or `case` used as a value that needs statements where it can't have them
    (§4.4): a branch of several statements, or a `case` subject that isn't a local or constant,
    anywhere but the whole right side of an assignment or a `return`; a branch that ends in a
    statement with no value when the assignment moves into the branches; and a branch of more
    than one expression in a constant's value.
  - `E0610`: a collection method (§4.12) where its value isn't the first thing its statement
    evaluates: anywhere but the whole right side of an assignment, the value of a `return` or
    a block, a loop's receiver, a `case`'s subject, or the start of a method chain in one.
  - `E0611`: a collection method's block that gives no value — empty, or ending in a statement
    with no value — or with `next` or `break` directly in it.
- **DSL keywords** (E06xx, §5):
  - `E0612`: a keyword anywhere but as a statement directly at the top level, or for a nested
    keyword, directly in its parent's block; a `def` or constant named after a keyword, its
    table, or its dispatch function.
  - `E0613`: an unknown keyword — a statement in a data keyword's block that isn't one of its
    nested keywords — or a named argument the keyword doesn't have.
  - `E0614`: arguments that don't fit: an id that isn't a symbol or string written out, more
    positional arguments than the keyword takes, a named argument given twice, a block the
    keyword doesn't take, block parameters where the block holds nested keywords, or a block
    or lambda with more parameters than its declared type.
  - `E0615`: something missing: an id, a block a handler needs, or a required field.
  - `E0616`: an id already used by the same keyword.

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

The `?:` at the end of `best_name` returns from each branch (§4.4):

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
  if best == nil then
    return "none"
  else
    return best["name"]                     -- best narrowed here too (C8)
  end
end

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
- **Empty hashes.** SPEC.md types `{}` as an empty array and has no way to write an empty map,
  so a hash needs at least one entry for now (`E0608`), and a keyword a script never uses has
  no table (§5). Options: type `{}` by its uses in the checker (the interpreter, which has no
  types, then needs to know which one it built), or an empty-map node only the Ruby translator
  makes. Either is a core change.
- **Defaults beyond literals.** A keyword's field defaults to `nil` or a literal (§5.4). A
  default function, such as `-> { true }` for a condition, would let the host call a field
  without checking for `nil`; it needs a default written in Ruby, which has no place in the
  script's source to point a diagnostic at.
- **Guard clauses.** `return 0 if x.nil?` narrows nothing after it (SPEC.md §5.3), which rules
  out a common Ruby idiom; narrowing after an early return is a core change that would serve
  Lua scripts equally.
- **Collection methods in conditions.** `if xs.any? { … }` is `E0610` (§4.12). An `if`'s first
  condition, a modifier's, and an `unless`'s are evaluated first, so their loops could run
  before the statement too; a `while`'s, an `elsif`'s, and a `?:`'s couldn't. Accepting the
  first kind makes the rule harder to state.
