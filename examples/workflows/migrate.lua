-- Migration: apply one mechanical change to each file, check each edit, then
-- run one build at the end.
--
-- Shape: pipeline(files, edit, check), then a single agent() after the
-- pipeline's barrier. Members edit different files concurrently, so keep the
-- change file-local; lower rness.workflow.max_concurrent_agents if needed.
--
-- meta: {"name":"migrate-logging","description":"Replace println! with tracing macros, file by file"}
-- args: {"files":["crates/foo/src/a.rs","crates/foo/src/b.rs"],"change":"Replace println!/eprintln! with tracing::info!/tracing::warn!","build":"cargo check -p foo"}

-- Editing needs a role with Edit/Write (the default flavor's worker role is
-- commented out in lua/agents.lua; examples/lua/roles.lua declares one).
local EDIT_ROLE, CHECK_ROLE = "worker", "reviewer"

local status = {
  type = "object",
  required = { "ok", "note" },
  properties = { ok = { type = "boolean" }, note = { type = "string" } },
}

local results = pipeline(args.files,
  function(file)
    local r = agent("In " .. file .. " only: " .. args.change .. ". Do not touch any other file. "
      .. "Report ok=false with a note if the change does not apply.",
      { label = file, phase = "Edit", role = EDIT_ROLE, schema = status })
    -- Returning nil skips the check stage; the item ends up nil.
    if r and r.ok then return r end
    return nil
  end,
  function(_, file)
    return agent("Read " .. file .. " and check that this change was applied correctly and "
      .. "completely: " .. args.change .. ". Report ok=false with the problem otherwise.",
      { label = file .. " check", phase = "Check", role = CHECK_ROLE, schema = status })
  end)

local migrated, needs_attention = {}, {}
for i, file in ipairs(args.files) do
  local r = results[i]
  if r and r.ok then
    migrated[#migrated + 1] = file
  else
    needs_attention[#needs_attention + 1] = { file = file, note = r and r.note or "edit failed or did not apply" }
  end
end

local build
if args.build and #migrated > 0 then
  phase("Build")
  build = agent("Run `" .. args.build .. "`. Report ok=false with the first errors if it fails.",
    { label = "build", role = EDIT_ROLE, schema = status })
end
return { migrated = migrated, needs_attention = needs_attention, build = build }
