-- The Lua twin of data.rb. A zero-based `xs[i]` is `xs[i + 1]` here.

MAX_HP = 100

function arrays(xs)
  xs[0 + 1] = 5
  xs[#xs + 1] = 6
  local f = xs[1]
  local l = xs[#xs]
  local n = #xs
  local v = xs[n - 1 + 1]
  if f == nil then
    return 0
  elseif l == nil then
    return 1
  else
    return f + l + n
  end
end

function record()
  local r = { name = "sword", damage = 5 }
  r.damage = r["damage"] + 1
  return r.damage
end

function hit(dmg)
  mem.hp = mem.hp - dmg
  mem.hp = math.min(math.max((mem.hp + 0), 0), MAX_HP)
  return mem:add_gold(1)
end

function maybe(s, c)
  local n
  if c then
    n = tonumber(s)
  end
  n = n or 0
  return n
end

function closures(n)
  local double = function(x) return x * 2 end
  return double(n) + double(1)
end

function pair(a, b)
  return a + b, a - b
end

function use_pair(a, b)
  local s, d = pair(a, b)
  s, d = d, s
  return s * d
end
