# The iteration forms and loops.

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
  m.each_pair { |k, v| n += v }
  m.each { |k, v| n += 1 if k == "a" }
  n
end

def counts(n)
  t = 0
  n.times { |i| t += i }
  (1..n).each { |i| t += i }
  (1...n).each { t += 1 }
  1.upto(n) { |i| t += i }
  1.step(n, 2) { |i| t += i }
  t
end

def loops(n)
  i = 0
  while i < n
    i += 1
  end
  i += 1 until i >= 10
  loop do
    break if i > 20
    i += 3
  end
  i -= 1 while i > 15
  i
end
