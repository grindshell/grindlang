-- A stat-calculation module (SPEC §9.1).
-- Demonstrates: exported const, mutually-recursive top-level functions, f64 math.

ARMOR_K = 100

---@param attack number
---@param armor number
---@return number
function mitigated(attack, armor)
  return attack * (ARMOR_K / (ARMOR_K + armor))
end

function lethal(attack, armor, hp)
  return mitigated(attack, armor) >= hp
end
