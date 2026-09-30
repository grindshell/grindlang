# Arrays, records, nil tests, memory, lambdas, and multiple values. Run with a memory `mem`
# holding `hp` and `gold`, a memory method `add_gold`, and `@` bound to `mem`.

MAX_HP = 100

def arrays(xs)
  xs[0] = 5
  xs << 6
  f = xs.first
  l = xs.last
  n = xs.length
  v = xs[n - 1]
  if f.nil?
    0
  elsif l.nil?
    1
  else
    f + l + n
  end
end

def record
  r = { name: "sword", damage: 5 }
  r.damage = r[:damage] + 1
  r.damage
end

def hit(dmg)
  @hp -= dmg
  @hp = (@hp + 0).clamp(0, MAX_HP)
  mem.add_gold(1)
end

def maybe(s, c)
  if c
    n = s.to_f
  end
  n ||= 0
  n
end

def closures(n)
  double = ->(x) { x * 2 }
  double.(n) + double.call(1)
end

def pair(a, b)
  return a + b, a - b
end

def use_pair(a, b)
  s, d = pair(a, b)
  s, d = d, s
  s * d
end
