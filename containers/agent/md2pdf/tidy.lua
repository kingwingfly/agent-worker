-- Repairs for the ways Markdown written by a language model tends to go wrong in print.

-- 1. HTML. It has no place in a PDF and pandoc would drop it, which is wrong for text
--    that was never meant as a tag: "Dear <customer name>" must not become "Dear ".
--      <br>                      becomes a line break (common inside table cells)
--      real HTML tags, comments  are removed, keeping what they enclose
--      anything else             is printed as written

local known = {}
for tag in ([[a abbr b blockquote body center code dd del details div dl dt em font
  h1 h2 h3 h4 h5 h6 head hr html i img ins kbd li mark ol p pre s small span strike strong
  sub summary sup table tbody td tfoot th thead tr tt u ul]]):gmatch("%S+") do
  known[tag] = true
end

-- Rewrites every <...> in a piece of raw HTML according to the rules above.
local function clean(html)
  html = html:gsub("<!%-%-.-%-%->", "")
  return (html:gsub("<[^<>]*>", function(tag)
    local name = tag:match("^</?%s*(%a%w*)")
    if name and name:lower() == "br" then return "\n" end
    if name and known[name:lower()] then return "" end
    return tag
  end))
end

-- Plain text as inlines, with its line breaks kept.
local function inlines(text)
  local result = pandoc.Inlines({})
  for line in text:gmatch("[^\n]*") do
    if #result > 0 then result:insert(pandoc.LineBreak()) end
    result:extend(pandoc.Inlines(line))
  end
  return result
end

function RawInline(el)
  if el.format ~= "html" then return nil end
  local text = clean(el.text)
  if text == "\n" then return pandoc.LineBreak() end
  return inlines(text)
end

function RawBlock(el)
  if el.format ~= "html" then return nil end
  local text = clean(el.text):gsub("^%s+", ""):gsub("%s+$", "")
  if text == "" then return {} end
  return pandoc.Para(inlines(text))
end

-- 2. Tables without a header. Markdown only recognises a table that starts with a header
--    row and a |---| line. Rows written on their own, typically totals set apart from
--    the table above them, would be printed as text full of pipes. A paragraph in which
--    every line is "| ... |" is turned into a table with no header instead.

local function trim(cell)
  while #cell > 0 and cell[1].t == "Space" do cell:remove(1) end
  while #cell > 0 and cell[#cell].t == "Space" do cell:remove(#cell) end
  return cell
end

-- Cuts one line of inlines into cells at the pipes in its plain text. Pipes inside
-- code spans or other markup are left alone.
local function cells_of(line)
  local cells, cell = {}, pandoc.Inlines({})
  for _, inline in ipairs(line) do
    if inline.t == "Str" and inline.text:find("|", 1, true) then
      local first = true
      for part in (inline.text .. "|"):gmatch("([^|]*)|") do
        if not first then
          cells[#cells + 1] = trim(cell)
          cell = pandoc.Inlines({})
        end
        if part ~= "" then cell:insert(pandoc.Str(part)) end
        first = false
      end
    else
      cell:insert(inline)
    end
  end
  cells[#cells + 1] = trim(cell)
  -- The pipes at both ends of the line leave an empty cell on each side.
  table.remove(cells, 1)
  table.remove(cells)
  return cells
end

function Para(el)
  local lines = { pandoc.Inlines({}) }
  for _, inline in ipairs(el.content) do
    if inline.t == "SoftBreak" or inline.t == "LineBreak" then
      lines[#lines + 1] = pandoc.Inlines({})
    else
      lines[#lines]:insert(inline)
    end
  end

  local rows, columns = {}, 0
  for _, line in ipairs(lines) do
    if not pandoc.utils.stringify(line):match("^%s*|.*|%s*$") then return nil end
    local cells = cells_of(line)
    if #cells == 0 then return nil end
    columns = math.max(columns, #cells)
    rows[#rows + 1] = cells
  end

  -- Columns that are empty in every row are padding, put there to line the rows up
  -- with a table above. They would only show as thin empty boxes.
  local kept = {}
  for column = 1, columns do
    for _, cells in ipairs(rows) do
      if cells[column] and #cells[column] > 0 then
        kept[#kept + 1] = column
        break
      end
    end
  end
  if #kept == 0 then return nil end

  local aligns, widths, headers, body = {}, {}, {}, {}
  for index = 1, #kept do
    aligns[index], widths[index], headers[index] = pandoc.AlignDefault, 0, {}
  end
  for _, cells in ipairs(rows) do
    local row = {}
    for index, column in ipairs(kept) do
      row[index] = { pandoc.Plain(cells[column] or pandoc.Inlines({})) }
    end
    body[#body + 1] = row
  end
  local result = pandoc.utils.from_simple_table(pandoc.SimpleTable({}, aligns, widths, headers, body))
  -- Without this the empty header is typeset as a blank first row.
  result.head.rows = {}
  return result
end
