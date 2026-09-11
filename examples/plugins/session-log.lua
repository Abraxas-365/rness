-- Add { name = "session-log", file = "plugins/session-log.lua" } to init.lua's
-- single rness.plugins.setup list after copying this file.
-- Log metadata only: never prompt text, tool arguments, or credentials.
rness.hook.on("turn_start", function(event)
  rness.log.info("turn started: " .. event.session)
end)
rness.hook.on("turn_end", function(event)
  rness.log.info("turn ended: " .. event.session)
end)
