-- Add { name = "text-tools", file = "plugins/text-tools.lua" } to init.lua's
-- single rness.plugins.setup list after copying this file.
-- A pure tool: no filesystem, network, or shell side effects.
rness.tool.register {
  name = "text_stats",
  description = "Count bytes, whitespace-delimited words, and lines in supplied text.",
  schema = {
    type = "object",
    properties = { text = { type = "string", description = "Text to measure" } },
    required = { "text" },
    additionalProperties = false,
  },
  run = function(args)
    assert(type(args.text) == "string", "text must be a string")
    assert(#args.text <= 1000000, "text exceeds the example's 1 MB limit")
    local words, newlines = 0, 0
    for _ in args.text:gmatch("%S+") do words = words + 1 end
    for _ in args.text:gmatch("\n") do newlines = newlines + 1 end
    local lines = #args.text == 0 and 0 or newlines + 1
    return string.format("bytes=%d words=%d lines=%d", #args.text, words, lines)
  end,
}
