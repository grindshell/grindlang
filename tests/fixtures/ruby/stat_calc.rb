# The worked example of SPEC-RUBY.md §7: a stat module.

XP_BASE = 10

private def curve(level)
  level * level
end

def xp_for(level)
  curve(level) * XP_BASE
end

# @param items {name: string, score: number}[]
def best_name(items)
  best = nil
  items.each do |item|
    if best.nil?
      best = item
    elsif item[:score] > best[:score]
      best = item
    end
  end
  best.nil? ? "none" : best[:name]
end
