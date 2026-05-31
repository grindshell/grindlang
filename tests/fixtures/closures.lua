-- A closures module.
-- Demonstrates: returning a closure that captures an enclosing local (upvalue), a closure
-- with a shared mutable upvalue, and a recursive `local function` closure.

-- Returns a function that adds `n` to its argument — `n` is captured as an upvalue.
function make_adder(n)
  return function(x)
    return x + n
  end
end

-- Returns a counter closure: each call increments and returns the shared upvalue `count`.
-- Two calls observe each other's writes (capture is by shared cell, not by value snapshot).
function make_counter(start)
  local count = start
  return function()
    count = count + 1
    return count
  end
end

-- A recursive `local function` closure: `fact` refers to itself through its own captured cell.
function factorial(n)
  local function fact(k)
    if k <= 1 then
      return 1
    end
    return k * fact(k - 1)
  end
  return fact(n)
end
