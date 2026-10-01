//! Randomized differential testing for the JIT (`PLAN.md` Phase 7 exit criterion: "interp
//! result == jit result for a large corpus, incl. fuzzed inputs").
//!
//! A small deterministic LCG generates argument vectors; for each program in a corpus we run
//! the AST interpreter, the IR `Vm`, and the cranelift `JitModule` over many random inputs
//! and assert all three agree (by `Display`). Deterministic so failures reproduce exactly.
//!
//! With the `ruby` feature, the corpus also holds Ruby-surface programs (`SPEC-RUBY.md`), each
//! with its Lua spelling, and the two programs' executors must all agree.

#![cfg(all(feature = "interp", feature = "jit"))]

use grindlang::codegen::JitModule;
use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::{RunError, TypeConfig, Value};

/// A tiny deterministic LCG (numerical-recipes constants) — no external rng, reproducible.
struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    /// A "nice" number in [-50, 50], sometimes whole — exercises both integer-ish and
    /// fractional paths.
    fn number(&mut self) -> f64 {
        let r = self.next_u64();
        let whole = (r % 101) as i64 - 50;
        if r & 0x100 == 0 {
            whole as f64
        } else {
            whole as f64 + ((r >> 16) % 1000) as f64 / 1000.0
        }
    }
}

/// All three executors for one program. The JIT compacts its value table at every loop back
/// edge after the loop creates a value (`JitModule::set_compact_slack(0)`), so the fuzzed loops
/// exercise compaction rather than only the default's rare one.
struct Triple {
    interp: Interpreter<'static>,
    vm: Vm<'static>,
    jit: JitModule,
    /// The program's source, shown when the executors disagree.
    src: String,
}

impl Triple {
    fn new(module: grindlang::Module, src: &str) -> Self {
        let cfg = TypeConfig::default();
        let module = Box::leak(Box::new(module));
        let (res, info) = grindlang::analyze_module(module, &cfg)
            .unwrap_or_else(|d| panic!("analyze failed:\n{}", d.render(src)));
        let res = Box::leak(Box::new(res));
        let info = Box::leak(Box::new(info));
        let program = Box::leak(Box::new(
            grindlang::ir::lower(module, res, info, &cfg).expect("lower"),
        ));
        grindlang::ir::verify(program).expect("verify");
        let mut jit = JitModule::compile(program).expect("jit");
        jit.set_compact_slack(0);
        Triple {
            interp: Interpreter::new(module, res).expect("interp"),
            vm: Vm::new(program),
            jit,
            src: src.to_string(),
        }
    }

    fn lua(src: &str) -> Self {
        let module =
            grindlang::parse(src).unwrap_or_else(|d| panic!("parse failed:\n{}", d.render(src)));
        Self::new(module, src)
    }

    #[cfg(feature = "ruby")]
    fn ruby(src: &str) -> Self {
        use grindlang::ruby::{RubyConfig, parse_module};
        let module = parse_module(src, &RubyConfig::new(), &TypeConfig::default())
            .unwrap_or_else(|d| panic!("translate failed:\n{}", d.render(src)));
        Self::new(module, src)
    }

    fn call(&mut self, func: &str, args: &[Value]) -> [(&'static str, Result<Value, RunError>); 3] {
        [
            ("AST", self.interp.call(func, args.to_vec())),
            ("IR", self.vm.call(func, args.to_vec())),
            ("JIT", self.jit.call(func, args.to_vec())),
        ]
    }
}

/// Fuzz one function over `iters` random numeric argument vectors of width `arity`, on every
/// executor of every program in `programs`, which must all agree: all give the same value (by
/// `Display`), or all fail.
fn fuzz(mut programs: Vec<Triple>, func: &str, arity: usize, seed: u64, iters: usize) {
    let mut rng = Lcg(seed);
    for _ in 0..iters {
        let args: Vec<Value> = (0..arity).map(|_| Value::Number(rng.number())).collect();
        let mut first: Option<(String, Option<String>)> = None;
        for p in &mut programs {
            for (oracle, got) in p.call(func, &args) {
                let got = got.ok().map(|v| v.to_string());
                match &first {
                    None => first = Some((format!("{oracle} of\n{}", p.src), got)),
                    Some((label, want)) => assert_eq!(
                        &got, want,
                        "{oracle} of\n{}\ndisagrees with {label}\non `{func}{args:?}`",
                        p.src
                    ),
                }
            }
        }
    }
}

/// Fuzz one function of Lua source `src`.
fn fuzz_numeric(src: &str, func: &str, arity: usize, seed: u64, iters: usize) {
    fuzz(vec![Triple::lua(src)], func, arity, seed, iters);
}

/// Fuzz one function of the Ruby-surface source `rb` and of `lua`, its Lua spelling.
#[cfg(feature = "ruby")]
fn fuzz_ruby(rb: &str, lua: &str, func: &str, arity: usize, seed: u64, iters: usize) {
    fuzz(
        vec![Triple::ruby(rb), Triple::lua(lua)],
        func,
        arity,
        seed,
        iters,
    );
}

#[test]
fn fuzz_arithmetic_mix() {
    let src = "function f(a, b, c)\n\
                 return (a + b) * c - a / (b + 1.0) + a % 7.0 + b // 3.0\n\
               end";
    fuzz_numeric(src, "f", 3, 0xC0FFEE, 500);
}

#[test]
fn fuzz_branches_and_compare() {
    let src = "function f(a, b)\n\
                 if a < b then return a * 2.0 end\n\
                 if a == b then return 0.0 end\n\
                 return b - a\n\
               end";
    fuzz_numeric(src, "f", 2, 0x1234_5678, 500);
}

#[test]
fn fuzz_loop_accumulate() {
    // A bounded loop whose trip count depends on the input.
    let src = "function f(n)\n\
                 local s = 0.0\n\
                 local i = 1\n\
                 while i <= n do\n\
                   s = s + i * 1.5\n\
                   i = i + 1\n\
                 end\n\
                 return s\n\
               end";
    fuzz_numeric(src, "f", 1, 0xABCD, 300);
}

#[test]
fn fuzz_math_builtins() {
    let src = "function f(a, b)\n\
                 return math.floor(a) + math.max(a, b) - math.min(a, b) + math.abs(a)\n\
               end";
    fuzz_numeric(src, "f", 2, 0x5EED, 500);
}

#[test]
fn fuzz_recursion() {
    // Clamp via comparison so deep/negative inputs terminate identically everywhere.
    let src = "function fib(n)\n\
                 if n < 2.0 then return n end\n\
                 if n > 20.0 then return 0.0 end\n\
                 return fib(n - 1.0) + fib(n - 2.0)\n\
               end";
    fuzz_numeric(src, "fib", 1, 0xF1B0, 200);
}

#[test]
fn fuzz_short_circuit() {
    let src = "function f(a, b)\n\
                 return (a < b) and (b < 10.0) or (a == 0.0)\n\
               end";
    fuzz_numeric(src, "f", 2, 0x5C2C, 500);
}

#[test]
fn fuzz_multi_return_unpack() {
    // A 3-value helper consumed by a parallel binding, combined back into a scalar. Exercises
    // `MakeTuple` (three elements) → `TupleGet` across all three oracles (`SPEC.md` §5.5).
    let src = "function stats(a, b)\n\
                 return a + b, a - b, a * b\n\
               end\n\
               function f(a, b)\n\
                 local s, d, p = stats(a, b)\n\
                 return s * 2.0 + d - p\n\
               end";
    fuzz_numeric(src, "f", 2, 0x7071E, 500);
}

#[test]
fn fuzz_multi_return_direct_tuple() {
    // A directly-returned tuple with a numeric and a bool element: the result crosses the host
    // boundary as a `Value::Tuple` and all three oracles must `Display` it identically — which
    // also stresses per-element box/unbox (bool element) through the tuple.
    let src = "function f(a, b)\n\
                 return a + b, a * b, a < b\n\
               end";
    fuzz_numeric(src, "f", 2, 0x7DEC0DE, 500);
}

// ---- Ruby-surface programs, each with its Lua spelling -----------------------------------

/// `case` over ranges giving a value, and `?:`, which become conditional expressions.
#[cfg(feature = "ruby")]
#[test]
fn fuzz_ruby_case_and_conditionals() {
    let rb = "\
def f(a, b)
  band = case a.floor
         when -50...-20 then b * 2
         when -20..0, 7 then a + b
         when 1...20 then a - b
         else a * b
         end
  band > 0 ? band : -band
end
";
    let lua = "\
function f(a, b)
  local s = math.floor(a)
  local band = 0
  if s >= -50 and s < -20 then
    band = b * 2
  elseif (s >= -20 and s <= 0) or s == 7 then
    band = a + b
  elseif s >= 1 and s < 20 then
    band = a - b
  else
    band = a * b
  end
  if band > 0 then return band end
  return -band
end
";
    fuzz_ruby(rb, lua, "f", 2, 0xCA5E, 500);
}

/// Collection methods, which become loops, and indexing from 0.
#[cfg(feature = "ruby")]
#[test]
fn fuzz_ruby_collections_and_indexing() {
    let rb = "\
def f(a, b)
  xs = [a, b, a * b, a - b, b - a]
  big = xs.select { |x| x > 0 }
  total = big.sum { |x| x * 2 }
  low = xs.find { |x| x < b }
  n = xs.count { |x| x.abs > 10 }
  any = xs.any? { |x| x == 0 }
  at = xs[a.abs.floor % 5]
  got = at.nil? ? -1 : at
  total + (low.nil? ? 0 : low) + n + got + (any ? 100 : 0)
end
";
    let lua = "\
function f(a, b)
  local xs = {a, b, a * b, a - b, b - a}
  local total = 0
  for _, x in ipairs(xs) do
    if x > 0 then total = total + x * 2 end
  end
  local low
  for _, x in ipairs(xs) do
    if x < b then
      low = x
      break
    end
  end
  local n = 0
  for _, x in ipairs(xs) do
    if math.abs(x) > 10 then n = n + 1 end
  end
  local any = false
  for _, x in ipairs(xs) do
    if x == 0 then
      any = true
      break
    end
  end
  local at = xs[math.floor(math.abs(a)) % 5 + 1]
  local got = -1
  if at ~= nil then got = at end
  local lowv = 0
  if low ~= nil then lowv = low end
  local bonus = 0
  if any then bonus = 100 end
  return total + lowv + n + got + bonus
end
";
    fuzz_ruby(rb, lua, "f", 2, 0xC011, 500);
}

/// The counting loops, `next`, and `break`.
#[cfg(feature = "ruby")]
#[test]
fn fuzz_ruby_loops() {
    let rb = "\
def f(n)
  k = n.abs.floor % 12
  s = 0
  k.times do |i|
    next if i % 3 == 0
    s += i
  end
  1.upto(k) { |i| s += i * 0.5 }
  z = 0
  z.step(k, 2) { |i| s -= i }
  [n, -n, n * 2].each_with_index do |x, i|
    break if i > 1
    s += x * i
  end
  j = 0
  while j < k
    j += 1
    next if j == 2
    s += 1
  end
  s
end
";
    let lua = "\
function f(n)
  local k = math.floor(math.abs(n)) % 12
  local s = 0
  for i = 0, k - 1 do
    if i % 3 == 0 then continue end
    s = s + i
  end
  for i = 1, k do s = s + i * 0.5 end
  for i = 0, k, 2 do s = s - i end
  for i, x in ipairs({n, -n, n * 2}) do
    if i - 1 > 1 then break end
    s = s + x * (i - 1)
  end
  local j = 0
  while j < k do
    j = j + 1
    if j == 2 then continue end
    s = s + 1
  end
  return s
end
";
    fuzz_ruby(rb, lua, "f", 1, 0x1007, 300);
}

/// Lambdas sharing a local, a method that gives `nil` where it falls off its end, and a
/// multiple assignment from a method returning two values.
#[cfg(feature = "ruby")]
#[test]
fn fuzz_ruby_closures_and_returns() {
    let rb = "\
def clip(a)
  if a > 10
    a
  elsif a < -10
    -a
  end
end

def split(a, b)
  return a + b, a - b
end

def f(a, b)
  scale = ->(x) { x * b }
  acc = 0
  add = ->(x) { acc += x }
  add.(scale.(a))
  add.call(a)
  c = clip(a)
  s, d = split(a, b)
  base = c.nil? ? s : c + d
  base + acc
end
";
    let lua = "\
function clip(a)
  if a > 10 then return a elseif a < -10 then return -a end
  return nil
end

function split(a, b)
  return a + b, a - b
end

function f(a, b)
  local scale = function(x) return x * b end
  local acc = 0
  local add = function(x) acc = acc + x end
  add(scale(a))
  add(a)
  local c = clip(a)
  local s, d = split(a, b)
  local base = s
  if c ~= nil then base = c + d end
  return base + acc
end
";
    fuzz_ruby(rb, lua, "f", 2, 0xC105, 500);
}

/// Interpolation, which goes through `tostring`, and string methods.
#[cfg(feature = "ruby")]
#[test]
fn fuzz_ruby_strings() {
    let rb = "\
def f(a, b)
  s = \"#{a.floor}:#{b > a}\"
  s.length + (s.empty? ? 1 : 0) + s.upcase.length
end
";
    let lua = "\
function f(a, b)
  local s = tostring(math.floor(a)) .. \":\" .. tostring(b > a)
  local e = 0
  if #s == 0 then e = 1 end
  return #s + e + #string.upper(s)
end
";
    fuzz_ruby(rb, lua, "f", 2, 0x5791, 300);
}
