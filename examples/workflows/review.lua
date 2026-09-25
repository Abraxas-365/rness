-- Review: look at one subject from several independent angles at once, then
-- merge the reviews into one prioritized list.
--
-- Shape: parallel({...}) followed by one agent() that combines the results.
-- parallel() is a barrier: the merge starts after every review has settled.
--
-- meta: {"name":"change-review","description":"Review the uncommitted changes from several angles, then merge"}
-- args: {"subject":"the uncommitted changes (run git diff HEAD)","angles":["correctness","security","performance","API design"]}

local ROLE = "reviewer" -- nil when rness.agents.allow_generic = true

phase("Review")
local thunks = {}
for i, angle in ipairs(args.angles) do
  -- Each thunk is a zero-argument function; parallel() runs them concurrently.
  thunks[i] = function()
    return agent("Review " .. args.subject .. " strictly for " .. angle .. ". "
      .. "List concrete issues with file:line evidence, most severe first. Say 'none' if there are none.",
      { label = angle, role = ROLE })
  end
end
local reviews = parallel(thunks)

phase("Merge")
local sections = {}
for i, angle in ipairs(args.angles) do
  -- A failed review is nil: say so instead of silently dropping the angle.
  sections[#sections + 1] = "## " .. angle .. "\n" .. (reviews[i] or "(this review failed)")
end
local merged = agent("Merge these reviews into one prioritized list. Drop duplicates and anything "
  .. "not backed by file:line evidence. Keep a note of angles whose review failed.\n\n"
  .. table.concat(sections, "\n\n"), { label = "merge", role = ROLE })

-- If the merge itself fails, return the raw reviews rather than nothing.
return merged or sections
