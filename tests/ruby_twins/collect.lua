-- The Lua twin of collect.rb.

function doubled(xs)
  local map1 = {}
  for _, x in ipairs(xs) do
    map1[#map1 + 1] = x * 2
  end
  return map1
end

-- The block assigns its parameter, so `select` keeps a hidden copy of each element.
function kept(xs)
  local select2 = {}
  for _, x_ in ipairs(xs) do
    local x = x_
    x = x * 10
    if x > 20 then
      select2[#select2 + 1] = x_
    end
  end
  return select2
end

function odds(xs)
  local reject3 = {}
  for _, x in ipairs(xs) do
    if x % 2 ~= 0 then
      reject3[#reject3 + 1] = x
    end
  end
  local ys = reject3
  return ys
end

function total(xs)
  local sum4 = 0
  for _, x_ in ipairs(xs) do
    sum4 = sum4 + x_
  end
  return sum4
end

function squares(n)
  local sum5 = 0
  for i = 1, n do
    sum5 = sum5 + i * i
  end
  return sum5
end

function big_count(xs)
  local count6 = 0
  for _, x in ipairs(xs) do
    if x > 2 then
      count6 = count6 + 1
    end
  end
  return count6
end

function any_negative(xs)
  local any7 = false
  for _, x in ipairs(xs) do
    if x < 0 then
      any7 = true
      break
    end
  end
  return any7
end

function all_positive(xs)
  local all8 = true
  for _, x in ipairs(xs) do
    if not (x > 0) then
      all8 = false
      break
    end
  end
  return all8
end

function first_big(xs)
  local find9 = nil
  for _, x in ipairs(xs) do
    if x > 2 then
      find9 = x
      break
    end
  end
  local found = find9
  if found == nil then
    return 0
  else
    return found
  end
end

function chained(xs)
  local select10 = {}
  for _, x in ipairs(xs) do
    if x > 1 then
      select10[#select10 + 1] = x
    end
  end
  local map11 = {}
  for _, x in ipairs(select10) do
    map11[#map11 + 1] = x * 10
  end
  return #map11
end
