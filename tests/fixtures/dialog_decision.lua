-- A dialog-tree decision module (SPEC §9.2).
-- `mem` is host-provided persistent memory: record{ reputation: number, met_elder: bool }.
-- Demonstrates: bool conditions, host memory read/write, array building, curated export.

---@return string
function elder_greeting()
  if not mem.met_elder then
    mem.met_elder = true
    return "intro"
  end
  if mem.reputation >= 50 then
    return "warm"
  end
  return "neutral"
end

function choices()
  local out = { "ask_quest", "leave" }
  if mem.reputation >= 50 then
    out[#out + 1] = "ask_favor"
  end
  return out
end

return {
  greeting = elder_greeting,
  choices = choices,
}
