local function trim(value)
  return value:match("^%s*(.-)%s*$")
end

local function supported(config)
  local caps = config.selection and rness.session.model_capabilities(config.selection)
  if type(caps) ~= "table" then caps = {} end
  local reasoning = type(caps.reasoning) == "table" and caps.reasoning or {}
  return {
    temperature = caps.temperature == true,
    ["max-output-tokens"] = caps.output_token_limit ~= false
      and (caps.output_token_limit == true or type(caps.max_output_tokens) == "number"),
    reasoning = type(reasoning.efforts) == "table" and #reasoning.efforts > 0,
    ["budget-tokens"] = type(reasoning.budget_tokens) == "table",
  }
end

local function available_fields(config)
  local fields = { "profile", "model" }
  local support = supported(config)
  for _, field in ipairs({ "reasoning", "budget-tokens", "temperature", "max-output-tokens" }) do
    if support[field] then fields[#fields + 1] = field end
  end
  return fields
end

local function describe(config)
  local selection = config.selection
  local reasoning = config.reasoning
  if type(reasoning) == "table" then
    reasoning = reasoning.kind == "effort" and reasoning.effort or tostring(reasoning.tokens)
  end
  local support = supported(config)
  local lines = { "Model: " .. (selection and (selection.route .. "/" .. selection.model) or "runtime default") }
  if support.reasoning or support["budget-tokens"] then lines[#lines + 1] = "Reasoning: " .. (reasoning or "default") end
  if support.temperature then lines[#lines + 1] = "Temperature: " .. tostring(config.temperature or "default") end
  if support["max-output-tokens"] then lines[#lines + 1] = "Max output tokens: " .. tostring(config.max_output_tokens or "default") end
  return table.concat(lines, "\n")
end

local function update(session, field, value)
  local config = rness.session.config(session)
  value = trim(value)
  assert(value ~= "", "A value is required; use default to clear a setting")
  if field ~= "model" and field ~= "profile" and value ~= "default" then
    assert(supported(config)[field], "Setting is not declared supported for this model: " .. field)
  end
  if field == "profile" then
    local preset = rness.session.profile_config(value, config.selection and config.selection.route)
    config.selection, config.reasoning = preset.selection, preset.reasoning
    config.temperature, config.max_output_tokens = preset.temperature, preset.max_output_tokens
  elseif field == "model" then
    local route, model = value:match("^([^/%s]+)/(%S+)$")
    assert(route and model, "Use provider/model (model IDs may contain slashes)")
    config.selection = { route = route, model = model }
    local support = supported(config)
    if not support.temperature then config.temperature = nil end
    if not support["max-output-tokens"] then config.max_output_tokens = nil end
    if type(config.reasoning) == "table" then
      local field = config.reasoning.kind == "effort" and "reasoning" or "budget-tokens"
      if not support[field] then config.reasoning = nil end
    end
  elseif field == "reasoning" then
    config.reasoning = value ~= "default" and { kind = "effort", effort = value } or nil
  elseif field == "budget-tokens" then
    local number = tonumber(value)
    assert(value == "default" or (number and number > 0 and number % 1 == 0), "Budget must be a positive integer or default")
    config.reasoning = value ~= "default" and { kind = "budget_tokens", tokens = number } or nil
  elseif field == "temperature" or field == "max-output-tokens" then
    local number = tonumber(value)
    assert(value == "default" or (number and number == number and number < math.huge and number >= 0), "Value must be a finite nonnegative number or default")
    if field == "max-output-tokens" then
      assert(value == "default" or (number > 0 and number % 1 == 0), "Output limit must be a positive integer or default")
    end
    config[field == "temperature" and "temperature" or "max_output_tokens"] = number
  else
    error("Settings: reasoning, budget-tokens, temperature, max-output-tokens")
  end
  return describe(rness.session.config(session, config))
end

rness.commands.register {
  name = "model",
  description = "Show or change this session's provider/model",
  usage = "[provider/model]",
  complete = function() return rness.session.model_names() end,
  run = function(ctx)
    local value = trim(ctx.raw_input)
    return { message = value == "" and (describe(rness.session.config(ctx.session)) .. "\nUse /model provider/model.")
      or update(ctx.session, "model", value) }
  end,
}

rness.commands.register {
  name = "model-settings",
  description = "Show or change this session's generation settings",
  usage = "[reasoning|budget-tokens|temperature|max-output-tokens value|default]",
  complete = function(ctx)
    local config = rness.session.config(ctx.session)
    local choices = {}
    for _, field in ipairs(available_fields(config)) do
      if field ~= "profile" and field ~= "model" then
        choices[#choices + 1] = field
        choices[#choices + 1] = field .. " default"
      end
    end
    local caps = config.selection and rness.session.model_capabilities(config.selection)
    if type(caps) == "table" and type(caps.reasoning) == "table"
      and type(caps.reasoning.efforts) == "table" then
      for _, effort in ipairs(caps.reasoning.efforts) do
        choices[#choices + 1] = "reasoning " .. effort
      end
    end
    return choices
  end,
  run = function(ctx)
    local input = trim(ctx.raw_input)
    if input == "" then return { message = describe(rness.session.config(ctx.session)) .. "\nUse default to clear an override." } end
    local field, value = input:match("^(%S+)%s+(.+)$")
    assert(field, "Usage: /model-settings setting value|default")
    return { message = update(ctx.session, field, value) }
  end,
}

rness.commands.register {
  name = "profile",
  description = "Apply a profile to this session without changing its definition",
  usage = "[name]",
  complete = function() return rness.session.profiles() end,
  run = function(ctx)
    local name = trim(ctx.raw_input)
    if name == "" then return { message = "Profiles: " .. table.concat(rness.session.profiles(), ", ") } end
    return { message = update(ctx.session, "profile", name) }
  end,
}
