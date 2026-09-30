# The collection methods with blocks, which become loops (SPEC-RUBY.md §4.12).

def doubled(xs)
  xs.map { |x| x * 2 }
end

def kept(xs)
  xs.select do |x|
    x = x * 10
    x > 20
  end
end

def odds(xs)
  ys = xs.reject { |x| x % 2 == 0 }
  ys
end

def total(xs)
  xs.sum
end

def squares(n)
  (1..n).sum { |i| i * i }
end

def big_count(xs)
  xs.count { |x| x > 2 }
end

def any_negative(xs)
  xs.any? { |x| x < 0 }
end

def all_positive(xs)
  xs.all? { |x| x > 0 }
end

def first_big(xs)
  found = xs.find { |x| x > 2 }
  found.nil? ? 0 : found
end

def chained(xs)
  xs.select { |x| x > 1 }.map { |x| x * 10 }.length
end
