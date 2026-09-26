-- Automatic session titles (dsh session-title-first-prompt-llm /
-- session-title-all-prompts-llm). The engine only provides the mechanism —
-- normalize, pin rules, one bounded model request; this plugin decides
-- when to title. Unload it (or set mode = "off") and sessions keep
-- whatever title they have; /title still works.
--
--   { name = "session-title", file = "plugins/session-title.lua", opts = {
--       mode = "first",        -- "first": LLM title from the first prompt
--                              -- "all":   regenerate from every prompt so far
--                              -- "off":   fallback title only
--       fallback = true,       -- first words of the first prompt, at once
--       profile = "fast",      -- model profile (default: session model)
--       timeout = 60, max_bytes = 80,
--   } }
--
-- Root sessions only: delegated children are labelled by their agent.
-- A pinned (user) title is never replaced; `/title unpin` releases it.

return function(opts)
  opts = opts or {}
  local mode = opts.mode or "first"
  if mode ~= "first" and mode ~= "all" and mode ~= "off" then
    error("session-title: mode must be first|all|off, got " .. tostring(mode))
  end
  local fallback = opts.fallback ~= false
  local max_bytes = opts.max_bytes or 80
  local request = {
    prompts = mode == "all" and "all" or "first",
    profile = opts.profile,
    timeout = opts.timeout,
    max_bytes = max_bytes,
    max_input_bytes = opts.max_input_bytes,
    max_output_tokens = opts.max_output_tokens,
  }

  -- Latest prompt wins: a newer request supersedes one still in flight.
  local inflight = {}

  local function pinned(session)
    local info = rness.session.title_info(session)
    return info ~= nil and info.source == "user"
  end

  rness.hook.on("prompt", function(ev)
    -- Root sessions carry `parent` = JSON null (a userdata sentinel, not
    -- nil); delegated children carry the parent id.
    if type(ev.parent) == "string" and ev.parent ~= "" then
      return
    end
    local session = ev.session
    if ev.index == 1 and fallback then
      local title = rness.session.title_fallback(ev.text, 5, math.min(64, max_bytes))
      if title ~= "" then
        rness.session.offer_title(session, title, "fallback", max_bytes)
      end
    end
    if mode == "off" or (mode == "first" and ev.index ~= 1) or pinned(session) then
      return
    end
    if inflight[session] then
      rness.task.cancel(inflight[session])
    end
    local id
    id = rness.task.spawn(function()
      local ok, title = pcall(rness.session.generate_title, session, request)
      if inflight[session] == id then
        inflight[session] = nil
      end
      if ok then
        rness.session.offer_title(session, title, "model", max_bytes)
      else
        rness.log.warn("session-title: " .. tostring(title))
      end
    end)
    inflight[session] = id
  end)
end
