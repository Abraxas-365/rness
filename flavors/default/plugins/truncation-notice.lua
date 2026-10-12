-- Truncation notice: when the provider cuts a reply at its output limit
-- (stop = "max_tokens"), keep the turn going with a steer telling the model
-- what happened, instead of closing the turn silently.
--
-- The provider drops a tool call whose arguments were cut mid-stream (it is
-- unsafe to run), so a truncated `Write` of a huge file leaves an empty
-- reply. Without this notice the model never learns why, and "continue"
-- retries the same oversized call.
--
-- Injected as a user-role steer (source: hook, tag: "truncated"). The default
-- theme shows the "truncated" tag; hide it with
-- `user.sources.hook.tags.truncated = { visible = false }`.
--
-- Configure via init.lua:
--   rness.truncation_notice = {
--     max_retries = 2,   -- consecutive truncations per turn to steer; then stop
--     message = "...",   -- override the steer text
--   }

local config = rness.truncation_notice or {}
local max_retries = config.max_retries or 2
if type(max_retries) ~= "number" or max_retries < 0 then
  error("rness.truncation_notice.max_retries must be a number >= 0, got " .. tostring(max_retries))
end
local message = config.message
  or "Your previous reply was cut off because it hit the output token limit; "
  .. "any tool call that was cut mid-arguments was discarded and did NOT run. "
  .. "Split the work into smaller pieces: e.g. create a file with only its first "
  .. "sections, then append the rest with Edit in further calls, one part at a time."

-- Consecutive truncations per session turn: "<session>:<turn>" -> count.
local streak = {}

rness.hook.on("turn_stopping", function(ev, next)
  local key = tostring(ev.session) .. ":" .. tostring(ev.turn)
  if ev.stop ~= "max_tokens" then
    streak[key] = nil
    return next()
  end
  local count = (streak[key] or 0) + 1
  streak[key] = count
  if count > max_retries then
    streak[key] = nil
    return next()
  end
  return { kind = "continue", messages = { { text = message, tag = "truncated" } } }
end)
