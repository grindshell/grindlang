# Data keywords (SPEC-RUBY.md §5.2): a small dialog tree.

node :shop do
  say "What'll it be?"
  choice "Buy a potion", if: -> { @gold >= 5 } do
    @gold -= 5
  end
  choice "Leave"
end

node :greeting do
  say "Welcome back, #{@name}!"
  choice "Shop", goto: :shop
  choice "Goodbye"
end
