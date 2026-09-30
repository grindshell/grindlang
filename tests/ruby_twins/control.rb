# `next`, and the `if`, `case`, and `?:` forms that become statements (SPEC-RUBY.md §4.4).

def odd_sum(xs)
  total = 0
  xs.each do |x|
    next if x % 2 == 0
    total += x
  end
  total
end

def count_up(n)
  i = 0
  hits = 0
  while i < n
    i += 1
    next if i % 3 == 0
    hits += 1
  end
  hits
end

def pick(c)
  c ? "yes" : "no"
end

def steps(n)
  g = "?"
  g = case n
      when 1
        t = "one"
        t
      else
        "many"
      end
  g
end

def bonus(crit, base)
  total = if crit
    mult = 2
    base * mult
  else
    base
  end
  total.nil? ? 0 : total
end

def spelled(n)
  return case n when 1 then "one" else "many" end
end
