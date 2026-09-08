-- Enable with rness.plugins.load("session-log") in init.lua.
-- Log metadata only: never prompt text, tool arguments, or credentials.
rness.hook.on("turn_start", function(event)
  rness.log.info("turn started: " .. event.session)
end)
rness.hook.on("turn_end", function(event)
  rness.log.info("turn ended: " .. event.session)
end)
