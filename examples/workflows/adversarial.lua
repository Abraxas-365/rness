-- Adversarial verification: one agent proposes candidate bugs, then a
-- skeptic tries to disprove each candidate independently. Only claims that
-- survive come back.
--
-- Shape: agent() → pipeline(candidates, disprove) → filter.
--
-- meta: {"name":"bug-hunt","description":"Find candidate bugs, then try to disprove each one"}
-- args: {"scope":"crates/rness-engine/src/workflow","max_candidates":8}

local FIND_ROLE, SKEPTIC_ROLE = "scout", "reviewer" -- nil for generic children

local candidates = {
  type = "object",
  required = { "candidates" },
  properties = {
    candidates = {
      type = "array",
      items = {
        type = "object",
        required = { "claim", "location" },
        properties = {
          claim = { type = "string", description = "A concrete failure scenario" },
          location = { type = "string", description = "file:line" },
        },
      },
    },
  },
}
local verdict = {
  type = "object",
  required = { "holds", "reason" },
  properties = { holds = { type = "boolean" }, reason = { type = "string" } },
}

phase("Find")
local found = agent("Find up to " .. (args.max_candidates or 8) .. " likely bugs in " .. args.scope
  .. ". Each claim must describe a concrete input or sequence that fails.",
  { label = "find", role = FIND_ROLE, schema = candidates })
-- Without candidates there is nothing to verify. An uncaught error() ends the
-- whole run with this message (and cancels anything still running).
if not found then error("the finder did not report candidates") end
log(#found.candidates .. " candidates")

phase("Disprove")
local verdicts = pipeline(found.candidates, function(c)
  return agent("Try hard to DISPROVE this claim by reading the code and, if useful, writing a quick "
    .. "check. Set holds=true only if you could not disprove it.\nClaim: " .. c.claim
    .. "\nLocation: " .. c.location,
    { label = c.location, role = SKEPTIC_ROLE, schema = verdict })
end)

local confirmed = {}
for i, c in ipairs(found.candidates) do
  local v = verdicts[i] -- nil when that skeptic failed: treat as unverified
  if v and v.holds then
    confirmed[#confirmed + 1] = { claim = c.claim, location = c.location, reason = v.reason }
  end
end
return { checked = #found.candidates, confirmed = confirmed }
