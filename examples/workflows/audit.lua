-- Audit: scan every target, then have a second agent verify each target's
-- findings. Only confirmed findings come back.
--
-- Shape: pipeline(targets, scan, verify). There is no barrier between the
-- stages: target 2 can still be scanning while target 1 is being verified.
--
-- meta: {"name":"unwrap-audit","description":"Find unwrap()/expect() in non-test code and verify each finding"}
-- args: {"targets":["crates/rness-engine","crates/rness-tools"],"question":"Where does non-test code call unwrap() or expect() on a value that can fail at runtime?"}

-- Roles you declared with subagent = true. Use nil for both when
-- rness.agents.allow_generic = true and you have no roles.
local SCAN_ROLE, VERIFY_ROLE = "scout", "reviewer"

-- Schemas turn free text into tables. The member must report a value that
-- matches through its structured_output tool; otherwise agent() returns nil.
local finding = {
  type = "object",
  required = { "file", "line", "why" },
  properties = {
    file = { type = "string" },
    line = { type = "integer" },
    why = { type = "string", description = "One sentence: how it can fail" },
  },
}
local report = {
  type = "object",
  required = { "findings" },
  properties = { findings = { type = "array", items = finding } },
}

local results = pipeline(args.targets,
  -- Stage 1 receives (item, item, index).
  function(target)
    return agent(args.question .. "\nScope: " .. target .. "\nReport every occurrence you find.", {
      label = target, phase = "Scan", role = SCAN_ROLE, schema = report,
    })
  end,
  -- Later stages receive (previous stage result, item, index). A nil result
  -- from stage 1 (failed member) skips this stage for that target.
  function(scan, target)
    if #scan.findings == 0 then return { target = target, confirmed = {} } end
    local verdict = agent(
      "Verify each finding below by reading the code. Keep only the real ones; drop false positives.\n"
        .. json.encode(scan.findings),
      { label = target .. " verify", phase = "Verify", role = VERIFY_ROLE, schema = report })
    if not verdict then return nil end
    log(string.format("%s: %d of %d confirmed", target, #verdict.findings, #scan.findings))
    return { target = target, confirmed = verdict.findings }
  end)

-- results may contain nil holes, and ipairs stops at the first one:
-- index by position instead.
local confirmed, failed = {}, {}
for i, target in ipairs(args.targets) do
  local r = results[i]
  if r then
    for _, f in ipairs(r.confirmed) do confirmed[#confirmed + 1] = f end
  else
    failed[#failed + 1] = target
  end
end
return { confirmed = confirmed, failed_targets = failed }
