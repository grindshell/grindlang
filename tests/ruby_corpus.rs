//! The Ruby corpus (`PLAN.md` Phase 10, R2): Ruby-surface programs, translated into the shared
//! tree, must run the same on every executor, and give the answers Ruby would.

#![cfg(all(feature = "ruby", feature = "interp", feature = "jit"))]

mod common;

use std::collections::BTreeMap;

use common::Executors;
use grindlang::ruby::{RubyConfig, parse_module};
use grindlang::{FnType, Type, TypeConfig, Value};

fn num(n: f64) -> Value {
    Value::Number(n)
}

fn string(s: &str) -> Value {
    Value::string(s.to_string())
}

fn nums(xs: &[f64]) -> Value {
    Value::array(xs.iter().map(|&x| num(x)).collect())
}

fn ruby_with(src: &str, cfg: &TypeConfig, ruby: &RubyConfig) -> Executors {
    let module = parse_module(src, ruby, cfg)
        .unwrap_or_else(|d| panic!("translate failed:\n{}", d.render(src)));
    Executors::from_module(module, cfg, src)
}

fn ruby(src: &str) -> Executors {
    ruby_with(src, &TypeConfig::default(), &RubyConfig::new())
}

#[test]
fn methods_constants_and_implicit_return() {
    let mut ex = ruby(
        r##"
XP_BASE = 10

private def curve(level)
  level * level
end

def xp_for(level)
  curve(level) * XP_BASE
end

def power(x)
  x ** 2 + x.div(3) + x % 4 - 7 / 2
end

def negatives(x)
  -x.abs + -2.abs
end
"##,
    );
    ex.assert_returns("xp_for", vec![num(3.0)], "90");
    ex.assert_returns("power", vec![num(7.0)], "50.5");
    ex.assert_returns("negatives", vec![num(-3.0)], "-1");
    ex.assert_returns("XP_BASE", vec![], "10");
    // `curve` is private: left out of the exports, still callable inside.
    for (oracle, got) in ex.call("curve", vec![num(2.0)]) {
        assert!(got.is_err(), "{oracle} called the private `curve`");
    }
}

#[test]
fn branches_and_loops() {
    let mut ex = ruby(
        r##"
def grade(score)
  case score
  when 90..100 then "A"
  when 80...90 then "B"
  when 0, -1 then "none"
  else "C"
  end
end

def classify(n)
  if n < 0
    "negative"
  elsif n == 0
    "zero"
  else
    "positive"
  end
end

def sign(n)
  return -1 if n < 0
  return 1 unless n <= 0
  0
end

def countdown(n)
  total = 0
  while n > 0
    total += n
    n -= 1
  end
  total
end

def until_loop(n)
  i = 0
  i += 1 until i >= n
  i
end

def first_multiple(n, k)
  i = 1
  loop do
    break if i % k == 0 && i >= n
    i += 1
  end
  i
end

def logic(a, b)
  if a and not b or !a && b
    "one"
  else
    "both or neither"
  end
end
"##,
    );
    for (score, want) in [
        (95.0, "A"),
        (90.0, "A"),
        (85.0, "B"),
        (0.0, "none"),
        (-1.0, "none"),
    ] {
        ex.assert_returns("grade", vec![num(score)], want);
    }
    ex.assert_returns("grade", vec![num(89.5)], "B");
    ex.assert_returns("grade", vec![num(50.0)], "C");
    ex.assert_returns("classify", vec![num(-2.0)], "negative");
    ex.assert_returns("classify", vec![num(0.0)], "zero");
    ex.assert_returns("sign", vec![num(-5.0)], "-1");
    ex.assert_returns("sign", vec![num(5.0)], "1");
    ex.assert_returns("sign", vec![num(0.0)], "0");
    ex.assert_returns("countdown", vec![num(4.0)], "10");
    ex.assert_returns("until_loop", vec![num(3.0)], "3");
    ex.assert_returns("first_multiple", vec![num(10.0), num(4.0)], "12");
    ex.assert_returns("logic", vec![Value::Bool(true), Value::Bool(false)], "one");
    ex.assert_returns(
        "logic",
        vec![Value::Bool(true), Value::Bool(true)],
        "both or neither",
    );
}

#[test]
fn iteration() {
    let mut ex = ruby(
        r##"
def sum(xs)
  total = 0
  xs.each { |x| total += x }
  total
end

def weighted(xs)
  total = 0
  xs.each_with_index do |x, i|
    total += x * i
  end
  total
end

# @param m { [string]: number }
def tally(m)
  n = 0
  m.each { |k, v| n += v }
  m.each_pair do |k, v|
    n += 1 if k == "b"
  end
  n
end

def triangle(n)
  t = 0
  n.times { |i| t += i }
  n.times { t += 100 }
  t
end

def ranges(a, b)
  s = 0
  (a..b).each { |i| s += i }
  (a...b).each { |i| s += i * 10 }
  a.upto(b) { |i| s += i * 100 }
  a.step(b, 2) { |i| s += i * 1000 }
  s
end

# @param want number
def found(xs, want)
  hit = false
  xs.each do |x|
    if x == want
      hit = true
      break
    end
  end
  hit
end
"##,
    );
    ex.assert_returns("sum", vec![nums(&[1.0, 2.0, 3.0])], "6");
    ex.assert_returns("weighted", vec![nums(&[5.0, 6.0, 7.0])], "20");
    let m = || {
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), num(2.0));
        m.insert("b".to_string(), num(3.0));
        Value::table(m)
    };
    ex.assert_returns("tally", vec![m()], "6");
    ex.assert_returns("triangle", vec![num(4.0)], "406");
    // 1+2+3, (1+2)*10, (1+2+3)*100, (1+3)*1000
    ex.assert_returns("ranges", vec![num(1.0), num(3.0)], "4636");
    ex.assert_returns("found", vec![nums(&[1.0, 2.0]), num(2.0)], "true");
    ex.assert_returns("found", vec![nums(&[1.0, 2.0]), num(5.0)], "false");
}

#[test]
fn strings_nil_tests_and_methods() {
    let mut ex = ruby(
        r##"
# Interpolation takes any scalar, so it doesn't settle a parameter's type.
# @param name string
# @param hp number
def greet(name, hp)
  "#{name} has #{hp} HP"
end
"##,
    );
    ex.assert_returns("greet", vec![string("Ann"), num(7.0)], "Ann has 7 HP");

    let mut ex = ruby(
        r##"
def parse(s)
  n = s.to_f
  if n.nil?
    -1
  else
    n * 2
  end
end

def parse_unless(s)
  n = s.to_f
  unless n.nil?
    return n + 1
  end
  0
end

def shout(s)
  "#{s.upcase}/#{s.downcase}/#{s.length}/#{s.size}"
end

def numbers(x)
  x.floor + x.ceil + x.abs + x.clamp(0, 10)
end

# @param x number
def text(x)
  "#{x.to_s}!"
end

# @param x number
def formatted(x)
  format("%d items", x)
end

# @param xs number[]
def tests(xs, n)
  xs.empty? == false && n.zero?
end

def roots(x)
  Math.sqrt(x) + Math.pi - Math.pi
end
"##,
    );
    ex.assert_returns("parse", vec![string("21")], "42");
    ex.assert_returns("parse", vec![string("x")], "-1");
    ex.assert_returns("parse_unless", vec![string("1")], "2");
    ex.assert_returns("parse_unless", vec![string("x")], "0");
    ex.assert_returns("shout", vec![string("Hey")], "HEY/hey/3/3");
    ex.assert_returns("numbers", vec![num(12.5)], "47.5");
    ex.assert_returns("text", vec![num(2.0)], "2!");
    ex.assert_returns("formatted", vec![num(3.0)], "3 items");
    ex.assert_returns("tests", vec![nums(&[1.0]), num(0.0)], "true");
    ex.assert_returns("roots", vec![num(9.0)], "3");
}

#[test]
fn arrays_hashes_and_records() {
    let mut ex = ruby(
        r##"
def arrays
  xs = [10, 20, 30]
  xs[0] = 11
  xs << 40
  xs.push(50)
  xs
end

def ends(xs)
  f = xs.first
  l = xs.last
  unless f.nil?
    unless l.nil?
      return f * 100 + l
    end
  end
  0
end

def at(xs, i)
  v = xs[i]
  if v.nil?
    -1
  else
    v
  end
end

def record
  r = { name: "sword", damage: 5 }
  r.damage = r.damage + 1
  r[:damage] + r.damage
end

def lookup(key)
  m = { "fire" => 2, :ice => 3 }
  v = m[key]
  if v.nil?
    0
  else
    v
  end
end
"##,
    );
    ex.assert_returns("arrays", vec![], "[11, 20, 30, 40, 50]");
    ex.assert_returns("ends", vec![nums(&[1.0, 2.0, 3.0])], "103");
    ex.assert_returns("ends", vec![nums(&[])], "0");
    ex.assert_returns("at", vec![nums(&[5.0, 6.0]), num(0.0)], "5");
    ex.assert_returns("at", vec![nums(&[5.0, 6.0]), num(1.0)], "6");
    ex.assert_returns("at", vec![nums(&[5.0, 6.0]), num(2.0)], "-1");
    ex.assert_returns("at", vec![nums(&[5.0, 6.0]), num(-1.0)], "-1");
    ex.assert_returns("record", vec![], "12");
    ex.assert_returns("lookup", vec![string("ice")], "3");
    ex.assert_returns("lookup", vec![string("mud")], "0");
}

#[test]
fn lambdas_and_multiple_values() {
    let mut ex = ruby(
        r##"
ADDER = ->(a, b) { a + b }

def apply(n)
  double = ->(x) { x * 2 }
  inc = lambda { |x| x + 1 }
  double.(inc.call(n))
end

def add(a, b)
  ADDER.(a, b)
end

def counter
  n = 0
  bump = -> do
    n += 1
    n
  end
  bump.()
  bump.()
end

def divmod(a, b)
  return a.div(b), a % b
end

def use_divmod(a, b)
  q, r = divmod(a, b)
  q * 10 + r
end

def swap(a, b)
  a, b = b, a
  a * 10 + b
end
"##,
    );
    ex.assert_returns("apply", vec![num(4.0)], "10");
    ex.assert_returns("add", vec![num(2.0), num(3.0)], "5");
    ex.assert_returns("counter", vec![], "2");
    ex.assert_returns("use_divmod", vec![num(17.0), num(5.0)], "32");
    ex.assert_returns("swap", vec![num(1.0), num(2.0)], "21");
}

#[test]
fn top_level_code() {
    let mut ex = ruby(
        r##"
SQUARES = []
i = 1
while i <= 5
  SQUARES << i * i
  i += 1
end

LIMIT = i * 10

# `big` is first assigned in a branch and read after it, so it is declared at the top.
if i > 3
  big = true
end
BIG = !big.nil?

# Two `case`s whose subjects are evaluated once, into temporaries of their own.
kind = "other"
case i.floor
when 6 then kind = "six"
end
case (i + 1).floor
when 7 then kind = "#{kind}+"
end
KIND = kind

def square(n)
  v = SQUARES[n]
  if v.nil?
    0
  else
    v
  end
end

def limit
  LIMIT
end
"##,
    );
    ex.assert_returns("square", vec![num(0.0)], "1");
    ex.assert_returns("square", vec![num(4.0)], "25");
    ex.assert_returns("square", vec![num(5.0)], "0");
    ex.assert_returns("limit", vec![], "60");
    ex.assert_returns("SQUARES", vec![], "[1, 4, 9, 16, 25]");
    ex.assert_returns("BIG", vec![], "true");
    ex.assert_returns("KIND", vec![], "six+");
}

fn memory_cfg() -> (TypeConfig, RubyConfig) {
    let mut fields = BTreeMap::new();
    fields.insert("hp".to_string(), Type::Number);
    fields.insert("gold".to_string(), Type::Number);
    let mut memory = BTreeMap::new();
    memory.insert("mem".to_string(), Type::Record(fields));
    let mut methods = BTreeMap::new();
    let mut mem_methods = BTreeMap::new();
    mem_methods.insert(
        "add_gold".to_string(),
        FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        },
    );
    mem_methods.insert(
        "reset".to_string(),
        FnType {
            params: vec![],
            ret: Box::new(Type::Unit),
        },
    );
    methods.insert("mem".to_string(), mem_methods);
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory,
        methods,
    };
    (cfg, RubyConfig::new().ivar_memory("mem"))
}

#[test]
fn host_memory() {
    let (cfg, rb) = memory_cfg();
    let mut ex = ruby_with(
        r##"
MAX_HP = 100

def hit(dmg)
  @hp -= dmg
  @hp = 0 if @hp < 0
end

def heal(amount)
  @hp = (@hp + amount).clamp(0, MAX_HP)
end

def pay(n)
  mem.add_gold(n)
end

def wipe
  mem.reset
end

def status
  "#{@hp} hp, #{mem.gold} gold"
end
"##,
        &cfg,
        &rb,
    );
    let make = || {
        let mut m = BTreeMap::new();
        m.insert("hp".to_string(), num(50.0));
        m.insert("gold".to_string(), num(10.0));
        Value::table(m)
    };
    ex.set_memory("mem", make);
    ex.set_method("mem:add_gold", |args: &[Value]| {
        let Value::Table(t) = &args[0] else {
            unreachable!()
        };
        let Value::Number(n) = args[1] else {
            unreachable!()
        };
        let gold = match t.borrow().get("gold") {
            Some(Value::Number(g)) => *g,
            _ => 0.0,
        };
        t.borrow_mut()
            .insert("gold".to_string(), Value::Number(gold + n));
        Ok(Value::Number(gold + n))
    });
    ex.set_method("mem:reset", |args: &[Value]| {
        let Value::Table(t) = &args[0] else {
            unreachable!()
        };
        t.borrow_mut()
            .insert("gold".to_string(), Value::Number(0.0));
        Ok(Value::Nil)
    });
    ex.assert_returns("hit", vec![num(20.0)], "nil");
    ex.assert_returns("status", vec![], "30 hp, 10 gold");
    ex.assert_returns("hit", vec![num(50.0)], "nil");
    ex.assert_returns("heal", vec![num(500.0)], "nil");
    ex.assert_returns("pay", vec![num(5.0)], "15");
    ex.assert_memory("mem", "{gold = 15, hp = 100}");
    ex.assert_returns("wipe", vec![], "nil");
    ex.assert_returns("status", vec![], "100 hp, 0 gold");
}

#[test]
fn best_name_from_the_worked_example() {
    // SPEC-RUBY.md §7, with the final `?:` (R3) written as an `if`.
    let mut ex = ruby(
        r##"
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
  if best.nil?
    "none"
  else
    best[:name]
  end
end
"##,
    );
    let item = |name: &str, score: f64| {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), string(name));
        m.insert("score".to_string(), num(score));
        Value::table(m)
    };
    ex.assert_returns(
        "best_name",
        vec![Value::array(vec![
            item("a", 1.0),
            item("b", 3.0),
            item("c", 2.0),
        ])],
        "b",
    );
    ex.assert_returns("best_name", vec![Value::array(vec![])], "none");
}
