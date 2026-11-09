-- The pattern functions run in C, where the instruction hook does not reach: a pattern that
-- backtracks for long cannot be stopped. So each call is refused unless the worst case of its
-- backtracking (about n^(quantifiers + 1) steps, or n^quantifiers when anchored, times 2 for
-- every "?") stays under a fixed number of steps.
local s, log, huge = string, math.log, math.log(1e9)
local MAX_TEXT, MAX_PATTERN = 16384, 128

local function weight(p)
  local n, i, q, k = #p, 1, 0, 0
  local anchored = p:sub(1, 1) == '^'
  if anchored then i = 2 end
  while i <= n do
    local c, d = p:sub(i, i), p:sub(i + 1, i + 1)
    if c == '%' and d == 'b' then q = q + 1; i = i + 4
    elseif c == '%' and d == 'f' then i = i + 2
    elseif c == '(' or c == ')' then i = i + 1
    else
      if c == '%' then i = i + 2
      else
        if c == '[' then
          i = i + 1
          if p:sub(i, i) == '^' then i = i + 1 end
          repeat
            if p:sub(i, i) == '%' then i = i + 1 end
            i = i + 1
          until i > n or p:sub(i, i) == ']'
        end
        i = i + 1
      end
      -- an escaped class, a set and a literal take the same quantifier
      local m = p:sub(i, i)
      if m == '*' or m == '+' or m == '-' then q = q + 1; i = i + 1
      elseif m == '?' then k = k + 1; i = i + 1 end
    end
  end
  return anchored and q or q + 1, k
end

for _, name in ipairs({'find', 'match', 'gmatch', 'gsub'}) do
  local f = s[name]
  s[name] = function(subject, pattern, ...)
    if type(subject) == 'string' and #subject > MAX_TEXT then
      error('the text is too long for a pattern', 2)
    end
    if type(pattern) == 'string' then
      if #pattern > MAX_PATTERN then error('the pattern is too long', 2) end
      local plain = name == 'find' and select(2, ...)
      if not plain and type(subject) == 'string' then
        local e, k = weight(pattern)
        if e * log(#subject + 2) + k * 0.7 > huge then
          error('the pattern is too expensive for this text', 2)
        end
      end
    end
    return f(subject, pattern, ...)
  end
end
