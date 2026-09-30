-- The Lua twin of loops.rb.

function sum(xs)
  local total = 0
  for _, x in ipairs(xs) do total = total + x end
  return total
end

function weighted(xs)
  local total = 0
  for i1, x in ipairs(xs) do
    local i = i1 - 1
    total = total + x * i
  end
  return total
end

---@param m { [string]: number }
function tally(m)
  local n = 0
  for k, v in pairs(m) do n = n + v end
  for k, v in pairs(m) do
    if k == "a" then n = n + 1 end
  end
  return n
end

function counts(n)
  local t = 0
  for i = 0, n - 1 do t = t + i end
  for i = 1, n do t = t + i end
  for _ = 1, n - 1 do t = t + 1 end
  for i = 1, n do t = t + i end
  for i = 1, n, 2 do t = t + i end
  return t
end

function loops(n)
  local i = 0
  while i < n do
    i = i + 1
  end
  while not (i >= 10) do i = i + 1 end
  while true do
    if i > 20 then break end
    i = i + 3
  end
  while i > 15 do i = i - 1 end
  return i
end
