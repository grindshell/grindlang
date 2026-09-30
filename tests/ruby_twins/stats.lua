-- The Lua twin of stats.rb: what the translator should turn it into.

XP_BASE = 10

function curve(level)
  return level * level
end

function xp_for(level)
  return curve(level) * XP_BASE
end

---@param name string
---@param hp number
function describe(name, hp)
  if hp <= 0 then
    return tostring(name) .. " is down"
  elseif hp < 10 then
    return tostring(name) .. " is hurt (" .. tostring(hp) .. ")"
  else
    return tostring(name) .. ": " .. tostring(math.floor(hp)) .. " hp"
  end
end

function grade(score)
  if score >= 90 and score <= 100 then
    return "A"
  elseif score >= 80 and score < 90 then
    return "B"
  elseif score == 0 or score == 1 then
    return "none"
  else
    return "C"
  end
end

function bucket(x)
  do
    local subject = math.floor(x)
    if subject == 0 then
      return "zero"
    else
      return "many"
    end
  end
end

function check(x)
  if not (x > 0) then return 0 end
  return x // 2 + x % 3 + x ^ 2
end

return {
  XP_BASE = XP_BASE,
  xp_for = xp_for,
  describe = describe,
  grade = grade,
  bucket = bucket,
  check = check,
}
