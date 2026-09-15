-- Opt-in tools. Also enable a provider; add both files to ~/.rness/plugins.
-- { name = "session-search", file = "plugins/session-search.lua",
--   dependencies = { "session-search-sqlite" } }
-- Provider activation alone registers no tools. Disabled means no indexing.
local provider = rness.session.search_provider()

local session = { type = "string", minLength = 1,
  description = "Source session in the caller workspace. Defaults to current session for event tools." }
local cursor = { type = "string", maxLength = 128,
  description = "next_cursor from the previous page. Keep other arguments unchanged. Restart without it if stale." }
local limit = { type = "integer", minimum = 1, maximum = 100, description = "Page size, default 20." }
local event = { type = "string", minLength = 1, description = "Exact source event reference." }

-- Theme-aware cards: semantic styles inherit your configured colorscheme.
-- UI previews never change the full tool result sent to the model.
local titles = {
  session_search = "Session search", session_event_search = "Event search",
  session_event_read = "Event read", session_event_trace = "Event trace",
  session_trace = "Session trace",
}
local function preview(value, length)
  if type(value) ~= "string" then return "" end
  value = value:gsub("%c", " "):gsub("%s+", " ")
  local ok, cut = pcall(utf8.offset, value, length + 1)
  if not ok then return "[invalid UTF-8]" end
  return cut and value:sub(1, cut - 1) .. "…" or value
end
local function card(call)
  local args = type(call.args) == "table" and call.args or {}
  local output = type(call.output) == "string" and call.output or ""
  local presentation = type(call.presentation) == "table" and call.presentation or {}
  local running = presentation.status == "running"
  local status = call.is_error and "failed" or running and "running" or "done"
  local body = {}
  local function line(text, style)
    if text ~= "" then body[#body + 1] = { text = text, style = style or "tool_output" } end
  end
  if args.query then line('“' .. preview(args.query, 160) .. '”', "heading") end
  line("scope · " .. preview(args.session_id or (call.name == "session_search" and "workspace" or "current session"), 80), "dim")
  if args.event_ref then line("event · " .. preview(args.event_ref, 80), "dim") end
  local ok, data = pcall(rness.json.decode, output)
  local summary = status
  if call.is_error then
    line(preview(output, 600), "error")
  elseif running then
    line("Searching saved history…", "dim")
  elseif not ok or type(data) ~= "table" then
    line(preview(output, 600), "tool_output")
  elseif type(data.chunk) == "string" then
    summary = "JSON chunk · " .. #data.chunk .. " bytes"
    line(preview(data.chunk, 480), "code")
  else
    local items = type(data.items) == "table" and data.items or {}
    local searching = call.name == "session_search" or call.name == "session_event_search"
    summary = #items .. (searching and " matches" or " links")
    if #items == 0 then line(searching and "No matching events." or "No direct relationships.", "dim") end
    for i = 1, math.min(#items, 4) do
      local item = items[i]
      if type(item) == "table" then
        if searching then
          line(string.format("%d. %s · %s", i, preview(item.session_id, 48), preview(item.event_type, 48)), "tool_name")
          line(preview(item.snippet, 220))
          line(preview(item.event_ref, 48) .. " · " .. preview(item.surface, 24), "dim")
        else
          local parent = type(item.parent) == "table" and item.parent or {}
          line(preview(item.from or "session", 48) .. " → " .. preview(item.relationship, 32) .. " → " .. preview(item.to or parent.session, 48), "tool_name")
        end
      end
    end
    if #items > 4 then line("+ " .. (#items - 4) .. " more in the tool result", "dim") end
  end
  if ok and type(data) == "table" then
    if type(data.next_cursor) == "string" then line("More available · continue with next_cursor", "added") end
    if data.snapshot_truncated == true then line("Snapshot limit reached · narrow your search", "heading") end
  end
  local duration = tonumber(presentation.duration_ms)
  if duration then summary = summary .. string.format(" · %.0f ms", duration) end
  return {
    header = {
      left = { { text = titles[call.name] or "Session history", style = "tool_name" } },
      right = { { text = summary, style = call.is_error and "error" or running and "heading" or "added" } },
    },
    body = body,
  }
end

local function register(name, description, properties, required)
  -- Omit an empty required list: plain Lua {} encodes as a JSON object.
  if #required == 0 then required = nil end
  rness.ui.messagebox.tool_card(name, card)
  rness.tool.register {
    name = name, description = description,
    schema = { type = "object", properties = properties, required = required, additionalProperties = false },
    run = function(args, ctx)
      assert(ctx and type(ctx.session) == "string" and ctx.session ~= "", "execution session required")
      -- Only the engine supplies caller identity; target is authorized in Rust.
      return provider(ctx.session, name, args)
    end,
  }
end

for _, spec in ipairs {
  { "session_search", "Search saved events with a literal phrase; returns the strongest matching event per session in the caller workspace, including current session. Paginated. Source-local history, not duplicated ancestor history." },
  { "session_event_search", "Search saved events in one authorized session, default current session. Returns individual event hits, paginated. Includes reasoning, tool calls/results and audit text; encoded images/stream chunks are not searched." },
} do
  register(spec[1], spec[2], {
    query = { type = "string", minLength = 1, maxLength = 4096, description = "Literal phrase, not FTS syntax." },
    session_id = session, cursor = cursor, limit = limit,
    event_types = { type = "array", maxItems = 32, items = { type = "string" }, description = "Durable types, e.g. user/message, assistant/message, tool/result." },
    surfaces = { type = "array", maxItems = 3, items = { type = "string", enum = { "current", "shadowed", "log-only" } } },
    time_from = { type = "string", description = "Inclusive timezone-qualified RFC3339 event time." },
    time_to = { type = "string", description = "Inclusive timezone-qualified RFC3339 event time." },
  }, { "query" })
end

register("session_event_read", "Read original event JSON from authorized source-local JSONL. Returns chunks of at most 8192 Unicode characters; concatenate chunk fields using next_cursor for the full event. Does not open SQLite.",
  { session_id = session, event_ref = event, cursor = cursor }, { "event_ref" })
register("session_event_trace", "List direct durable replacement/source relationships to or from an event, paginated. Does not infer provenance or read parent sessions.",
  { session_id = session, event_ref = event, cursor = cursor, limit = limit }, { "event_ref" })
register("session_trace", "List the session fork reference and local compaction/source relationships, paginated. Does not recursively read ancestors or other sessions.",
  { session_id = session, cursor = cursor, limit = limit }, {})
