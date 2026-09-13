-- Copy to ~/.rness/plugins/delivery.lua and add this to your existing setup list:
-- { name = "delivery", file = "plugins/delivery.lua",
--   keys = { queue = "<F8>", steer = "<F9>" } }
-- keys.<slot> accepts a chord/list or false; keys=false disables all defaults.
-- Enter still queues; Ctrl+Enter still steers. Override those in keymap.setup.
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
