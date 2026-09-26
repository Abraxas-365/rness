-- Session titles, entirely in Lua. The engine only stores titles, keeps
-- them terminal-safe (normalized, byte-capped) and enforces pins; *what*
-- to ask the model, with which profile, and *when*, is all here.
--
--   { name = "title", file = "plugins/title.lua", opts = {
--       auto = "first",       -- "first": model title from the first prompt
--                             -- "all":   retitle on every prompt, from all prompts so far
--                             -- "off":   no automatic titles (/title still works)
--       fallback = true,      -- first words of the first prompt, at once
--       profile = "fast",     -- model profile (default: the session's model)
--       system = "…",         -- custom system prompt (default below)
--       timeout = 60, max_output_tokens = 64,
--       max_input_bytes = 4096, max_bytes = 80,
--   } }
--
-- /title [text | auto | unpin]: show, rename (pins), regenerate in the
-- background (pins; the result arrives as a notice), or release a pin so
-- automatic titles may replace it. Works while a turn runs. Automatic
-- titles only title root sessions and never replace a pinned title.
--
-- Want a different title entirely? Call rness.llm.complete yourself and
-- store the result with rness.session.title(id, text) (pins) or
-- rness.session.offer_title(id, text, "model") (respects pins).

local SYSTEM = [[Create a concise title for an AI coding-assistant session from the supplied human messages.
Return only the title on one line, **in plain text of natural language**, with no quotes, prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed.
Use the language of the messages.
Aim for about 5 words in non-CJK languages or 10 CJK characters.]]

local HEAD = "Generate the session title from this JSON array of human messages:\n"

-- Largest prefix of `s` of at most `n` bytes that ends on a UTF-8 boundary.
local function utf8_prefix(s, n)
  if #s <= n then return s end
  while n > 0 do
    local byte = s:byte(n + 1)
    if not byte or byte < 0x80 or byte >= 0xC0 then break end
    n = n - 1
  end
  return s:sub(1, n)
end

-- The request text within `max` bytes: drop the oldest prompts first, then
-- trim the oldest kept one (JSON escaping can grow text, so shrink to fit).
local function frame(prompts, max)
  local texts = {}
  for i, p in ipairs(prompts) do texts[i] = p end
  local function render()
    local messages = {}
    for i, t in ipairs(texts) do messages[i] = { text = t } end
    return HEAD .. rness.json.encode(messages)
  end
  while #texts > 1 and #render() > max do table.remove(texts, 1) end
  local framed = render()
  if #framed <= max then return framed end
  local keep = math.max(max - #HEAD - 16, 0)
  local first = texts[1]
  while true do
    texts[1] = utf8_prefix(first, keep)
    framed = render()
    if #framed <= max or texts[1] == "" then return framed end
    keep = #texts[1] * 3 // 4
  end
end

-- First `words` words of `text`, whitespace collapsed.
local function first_words(text, words)
  local out = {}
  for word in text:gmatch("%S+") do
    out[#out + 1] = word
    if #out == words then break end
  end
  return table.concat(out, " ")
end

return function(opts)
  opts = opts or {}
  local auto = opts.auto or "first"
  if auto ~= "first" and auto ~= "all" and auto ~= "off" then
    error("title: auto must be first|all|off, got " .. tostring(auto))
  end
  local fallback = opts.fallback ~= false
  local max_bytes = opts.max_bytes or 80
  local max_input = opts.max_input_bytes or 4096

  -- Ask the model for a title from `session`'s first (or all) prompts.
  -- Yields: call from a command, tool or rness.task.
  local function generate(session, all)
    local prompts = rness.session.prompts(session)
    if #prompts == 0 then error("no prompt to title yet", 0) end
    if not all then prompts = { prompts[1] } end
    return rness.llm.complete {
      session = session,
      system = opts.system or SYSTEM,
      prompt = frame(prompts, max_input),
      profile = opts.profile,
      timeout = opts.timeout or 60,
      max_output_tokens = opts.max_output_tokens or 64,
    }
  end

  -- One title request per session; a newer one supersedes it. `explicit`
  -- marks a /title auto request, which automatic titling never cancels.
  local inflight = {}

  local function cancel_inflight(session)
    local current = inflight[session]
    if current then
      rness.task.cancel(current.id)
      inflight[session] = nil
    end
  end

  -- Generate in the background; `done(ok, title_or_error)` runs after.
  local function start(session, explicit, done)
    cancel_inflight(session)
    local entry = { explicit = explicit }
    entry.id = rness.task.spawn(function()
      local ok, result = pcall(generate, session, auto == "all")
      if inflight[session] == entry then
        inflight[session] = nil
      end
      done(ok, result)
    end)
    inflight[session] = entry
  end

  local function tell(session, text)
    pcall(rness.session.notify, session, text)
  end

  rness.commands.register {
    name = "title",
    description = "View, rename, or auto-generate session title",
    usage = "[text | auto | unpin]",
    -- Titles are committed at the next step boundary, so /title works while
    -- a turn runs; /title auto returns at once and reports back later.
    allow_busy = true,
    run = function(ctx)
      local session = ctx.session
      local text = ctx.raw_input:match("^%s*(.-)%s*$")

      if text == "" then
        local info = rness.session.title_info(session)
        if not info then
          return { message = "(no title)" }
        end
        local pinned = info.source == "user" and " (pinned)" or ""
        return { message = info.title .. pinned }
      end

      if text == "auto" then
        if #rness.session.prompts(session) == 0 then
          return { message = "Title generation failed: no prompt to title yet" }
        end
        start(session, true, function(ok, result)
          if ok then
            ok, result = pcall(rness.session.title, session, utf8_prefix(result, max_bytes))
          end
          if ok then
            tell(session, "Title: " .. result)
          else
            tell(session, "Title generation failed: " .. tostring(result))
          end
        end)
        return { message = "Generating title…" }
      end

      if text == "unpin" then
        local info = rness.session.title_info(session)
        if not info or info.source ~= "user" then
          return { message = "Title is not pinned" }
        end
        cancel_inflight(session)
        rness.session.title(session, info.title, "model")
        return { message = "Title unpinned" }
      end

      cancel_inflight(session)
      local title = rness.session.title(session, text)
      return { message = "Title set: " .. title }
    end,
  }

  if auto == "off" and not fallback then return end

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
      local title = first_words(ev.text, 5)
      if title ~= "" then
        pcall(rness.session.offer_title, session, title, "fallback", math.min(64, max_bytes))
      end
    end
    if auto == "off" or (auto == "first" and ev.index ~= 1) or pinned(session) then
      return
    end
    if inflight[session] and inflight[session].explicit then
      return
    end
    start(session, false, function(ok, title)
      if ok then
        ok, title = pcall(rness.session.offer_title, session, title, "model", max_bytes)
      end
      if not ok then
        rness.log.warn("title: " .. tostring(title))
      end
    end)
  end)
end
