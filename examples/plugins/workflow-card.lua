-- workflow-card.lua — live progress card for the opt-in `workflow` tool.
--
-- Enable the tool in init.lua (`rness.workflow = {}`), copy this file to
-- plugins/, then add { name = "workflow-card", file = "plugins/workflow-card.lua" }
-- to the single rness.plugins.setup list. Without it the workflow call uses the
-- built-in tool card and shows no live progress.
--
-- While a run is active rness re-renders the card about five times a second
-- from call.presentation (kind = "workflow_activity"): name, description,
-- status, elapsed_ms, phase, total, counts{queued,running,completed,failed,
-- cancelled}, members[{label, phase, status, session}] (bounded, running first)
-- and logs (the last few log() lines). When the run ends the card is rebuilt
-- from the stored tool result: the same fields plus call.output.
--
-- Cards show 8 lines in preview mode by default; to show more, set
--   rness.ui.messagebox = { tools = { workflow = { display = "preview", preview_lines = 14 } } }

-- One-line summary that never splits a UTF-8 character.
local function preview(text, limit, fallback)
  if type(text) ~= "string" then return fallback or "" end
  text = text:gsub("%s+", " "):match("^%s*(.-)%s*$")
  if text == "" then return fallback or "" end
  local beyond = utf8.offset(text, limit + 1)
  if beyond and beyond <= #text then
    return text:sub(1, utf8.offset(text, limit) - 1) .. "…"
  end
  return text
end

local MARKS = { running = "●", queued = "○", completed = "✓", failed = "✗", cancelled = "–" }
local RESULT_LINES = 8 -- durable results can be large (max_result_chars)

rness.ui.tool_card("workflow", function(call)
  local meta = call.presentation
  -- No workflow metadata (an older session, or a validation error): let the
  -- built-in card render it.
  if type(meta) ~= "table" or meta.kind ~= "workflow_activity" then return nil end

  local status = preview(meta.status, 24, "unknown")
  local failed = call.is_error or status == "error"
  local elapsed = tonumber(meta.elapsed_ms)
  local duration = elapsed and elapsed >= 0 and elapsed < math.huge
    and string.format(" · %ds", math.floor(elapsed / 1000)) or ""
  local counts = type(meta.counts) == "table" and meta.counts or {}
  local function n(key) return tonumber(counts[key]) or 0 end

  local body = {}
  local description = preview(meta.description, 200)
  if description ~= "" then body[#body + 1] = { text = description, style = "dim" } end
  local phase = preview(meta.phase, 80)
  if phase ~= "" then body[#body + 1] = { text = "Phase: " .. phase, style = "tool_name" } end
  body[#body + 1] = {
    text = string.format("Agents: %d · %d running · %d queued · %d done · %d failed · %d cancelled",
      tonumber(meta.total) or 0, n("running"), n("queued"), n("completed"), n("failed"), n("cancelled")),
    style = n("failed") > 0 and "error" or "dim",
  }

  local members = type(meta.members) == "table" and meta.members or {}
  for _, member in ipairs(members) do
    if type(member) == "table" then
      local state = type(member.status) == "string" and member.status or "queued"
      local label = preview(member.label, 72, "agent")
      local group = preview(member.phase, 24)
      if group ~= "" then label = label .. " · " .. group end
      body[#body + 1] = {
        text = string.format("  %s %s", MARKS[state] or "?", label),
        style = state == "failed" and "error" or state == "running" and "tool_name" or "dim",
      }
    end
  end
  local total = tonumber(meta.total) or #members
  if total > #members then
    body[#body + 1] = { text = string.format("  … %d more", total - #members), style = "dim" }
  end
  for _, line in ipairs(type(meta.logs) == "table" and meta.logs or {}) do
    body[#body + 1] = { text = "› " .. preview(line, 160), style = "assistant_text" }
  end

  if status ~= "running" and type(call.output) == "string" and call.output ~= "" then
    local shown, style = 0, failed and "error" or "tool_output"
    for line in (call.output .. "\n"):gmatch("(.-)\r?\n") do
      if shown == RESULT_LINES then
        body[#body + 1] = { text = "…", style = "dim" }
        break
      end
      local beyond = utf8.len(line) and utf8.offset(line, 201)
      if beyond and beyond <= #line then line = line:sub(1, beyond - 1) .. "…" end
      body[#body + 1] = { text = line, style = style }
      shown = shown + 1
    end
  end

  return {
    header = {
      text = "workflow: " .. preview(meta.name, 48, "workflow") .. " · " .. status .. duration,
      style = failed and "error" or "tool_name",
    },
    body = body,
  }
end)
