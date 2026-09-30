# Handler keywords (SPEC-RUBY.md §5.1): event handlers kept in host memory.

MAX_HP = 100

on :hit do |dmg|
  @hp -= dmg
end

on :heal do |amount|
  @hp = (@hp + amount).clamp(0, MAX_HP)
end
