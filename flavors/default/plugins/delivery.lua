-- Queue and Steer submit the current prompt, including attachments.
-- Remap or disable either key slot in the delivery entry in plugins.setup.
return function(_, plugin)
  plugin.action("queue", {
    scope = "promptbox", description = "Queue: send after the current turn",
    run = function(ctx) ctx.promptbox.queue() end,
  })
  plugin.action("steer", {
    scope = "promptbox", description = "Steer: send at the next model step",
    run = function(ctx) ctx.promptbox.steer() end,
  })
  plugin.keys({
    queue = { action = "queue", key = "<F8>" },
    steer = { action = "steer", key = "<F9>" },
  })
end
