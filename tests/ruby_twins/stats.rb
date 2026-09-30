# Constants, private defs, interpolation, branches, and `case`.

XP_BASE = 10

private def curve(level)
  level * level
end

def xp_for(level)
  curve(level) * XP_BASE
end

# @param name string
# @param hp number
def describe(name, hp)
  if hp <= 0
    "#{name} is down"
  elsif hp < 10
    "#{name} is hurt (#{hp})"
  else
    "#{name}: #{hp.floor} hp"
  end
end

def grade(score)
  case score
  when 90..100 then "A"
  when 80...90 then "B"
  when 0, 1 then "none"
  else "C"
  end
end

def bucket(x)
  case x.floor
  when 0 then "zero"
  else "many"
  end
end

def check(x)
  return 0 unless x > 0
  x.div(2) + x % 3 + x ** 2
end
