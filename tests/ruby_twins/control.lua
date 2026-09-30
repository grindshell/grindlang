-- The Lua twin of control.rb.

function odd_sum(xs)
  local total = 0
  for _, x in ipairs(xs) do
    if x % 2 == 0 then
      continue
    end
    total = total + x
  end
  return total
end

function count_up(n)
  local i = 0
  local hits = 0
  while i < n do
    i = i + 1
    if i % 3 == 0 then
      continue
    end
    hits = hits + 1
  end
  return hits
end

function pick(c)
  if c then
    return "yes"
  else
    return "no"
  end
end

function steps(n)
  local g = "?"
  if n == 1 then
    local t = "one"
    g = t
  else
    g = "many"
  end
  return g
end

function bonus(crit, base)
  local total
  if crit then
    local mult = 2
    total = base * mult
  else
    total = base
  end
  if total == nil then
    return 0
  else
    return total
  end
end

function spelled(n)
  if n == 1 then
    return "one"
  else
    return "many"
  end
end
