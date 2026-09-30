# A tour of the grammar of SPEC-RUBY.md §2–3: every accepted form at least once.

RATES = { fire: 2, ice: 0.5, "earth" => 1_000 }
NAMES = [
  "slime",
  "bat",
]
limits = { lo: 0x10, hi: 2.5e3 }

private

# @param base number
# @param mult number
# @return number
def damage(base, mult)
  base * mult
end

public def describe(hp, max)
  label = case hp
          when 0 then :dead
          when 1...10, 10 then :low
          else :ok
          end
  pct = (hp / max * 100).floor
  "#{label}: #{pct}% (#{hp}/#{max})"
end

def classify(n)
  return "negative" if n < 0
  return "zero" unless n != 0
  if n.div(2) * 2 == n then "even" else "odd" end
end

def total(xs)
  sum = 0
  xs.each { |x| sum += x }
  i = 0
  while i < xs.length do
    i += 1
    next if xs[i].nil?
    break unless i < 100
  end
  until i == 0
    i -= 1
  end
  loop do
    break
  end
  3.times do |k|
    sum += k
  end
  (1..3).each { |k| sum += k ** 2 }
  sum
end

def pick(a, b)
  return b, a if a > b
  return a, b
end

def flags(x, y)
  ok = !x.nil? && (y || false)
  fallback = x || "none"
  first, second = pick(1, 2)
  first, second = second, first
  z = not_nil(x) ? -1 : -2.abs
  big = -2 ** 2
  ok and big < 0 or z == 1
end

def not_nil(x)
  !x.nil?
end

def builders
  double = ->(v) { v * 2 }
  triple = lambda { |v| v * 3 }
  noop = -> do
  end
  xs = [1, 2, 3]
  xs << 4
  xs.push(double.(5))
  xs[0] = triple.call(xs[1])
  ys = xs.map do |v|
    v + 1
  end
  evens = xs
    .select { |v| v % 2 == 0 }
    .count
  mem.gold += 1
  mem.add_item(:sword)
  @hp ||= 10
  @hp = Math.sqrt(@hp) +
    evens
  x = if ys.empty? then 0 else ys.first end
  y = ys.length > 0 ? ys.last : nil
  say "x: #{x}", 'y: raw\n'
  format("%d", x); say "done"
end
