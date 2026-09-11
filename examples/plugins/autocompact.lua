-- autocompact.lua — legacy idle-only, turn-end compaction policy.
-- Prefer rness.compaction in init.lua for pre-step checks and overflow recovery.
-- Do not enable both policies for the same route. This example uses reported
-- usage and cannot immediately remeasure after pruning.
--
-- Copy this file, then add { name = "autocompact", file = "plugins/autocompact.lua" }
-- to init.lua's single rness.plugins.setup list. Copying alone does not enable it.
--
-- Two layers, cheapest first (dsh compaction stack):
--   1. prune  — deterministic, instant, model-free: oversized tool
--               results get their middle cut (head + marker + tail)
--   2. compact — one summarizer model request folds old turns into a
--               checkpoint when pruning wasn't enough
--
-- Both are append-only: originals stay in the log, shadowed by
-- compaction/prune / compaction/summary events. Recent turns are never
-- touched (KEEP_TURNS stability rule).
--
-- Uses:
--   rness.hook.on("turn_end")           fires when any session's turn ends
--   rness.session.usage(id)             context tokens + turn count
--   rness.session.prune(id, opts)       -> pruned count
--   rness.session.compact(id, keep)     -> { shadowed=, summary= } (blocks)
--   rness.models.get(rness.model)       declared capabilities (models.lua)

-- Act when the last request passed 80% of the active model's declared
-- context window (models.lua); YOUR literal when undeclared. Same
-- number spinner.lua displays — keep them in sync.
local function max_input_tokens()
  local caps = rness.models.get(rness.model)
  if caps and caps.context_window then
    return math.floor(caps.context_window * 0.8)
  end
  return 150000
end

local KEEP_TURNS = 2 -- recent turns kept verbatim

-- dsh tool-result-pruner reference budget.
local PRUNE = { threshold = 8192, head = 4096, tail = 1024, keep_turns = KEEP_TURNS }

rness.hook.on("turn_end", function(ev)
  local ok, usage = pcall(rness.session.usage, ev.session)
  if not ok or usage.input < max_input_tokens() then return end

  -- Layer 1: free. Often enough when tool output is the bloat. Recorded
  -- usage only updates on the NEXT request, so after pruning we stop and
  -- let the next turn_end re-evaluate with fresh numbers.
  local pok, pruned = pcall(rness.session.prune, ev.session, PRUNE)
  if pok and pruned > 0 then
    rness.log.info(("autocompact: %s pruned %d tool results"):format(ev.session, pruned))
    return
  end

  -- Layer 2: summarize. One model request, folds old turns.
  if usage.turns <= KEEP_TURNS + 1 then return end -- nothing worth folding
  local done, report = pcall(rness.session.compact, ev.session, KEEP_TURNS)
  if done then
    rness.log.info(("autocompact: %s folded %d events"):format(ev.session, report.shadowed))
  else
    rness.log.warn("autocompact failed: " .. tostring(report))
  end
end)

-- ---------------------------------------------------------------------------
-- Variants. The policy is just Lua — copy one of these bodies over the
-- hook above (or combine them) instead of adding config knobs.

-- (a) Per-scope thresholds: subagent children get a tighter budget than
-- your main session (they burn context fast on tool output).
--
--   rness.hook.on("turn_end", function(ev)
--     local budget = max_input_tokens()
--     if rness.subagents.delegation(ev.session) then budget = 50000 end
--     local ok, usage = pcall(rness.session.usage, ev.session)
--     if ok and usage.input >= budget then
--       pcall(rness.session.compact, ev.session, KEEP_TURNS)
--     end
--   end)

-- (b) Subagents only: leave your own session alone, keep children lean.
--
--   rness.hook.on("turn_end", function(ev)
--     if not rness.subagents.delegation(ev.session) then return end
--     local ok, usage = pcall(rness.session.usage, ev.session)
--     if ok and usage.input >= 50000 then
--       pcall(rness.session.prune, ev.session, PRUNE)
--     end
--   end)

-- (c) Manual command instead of automatic: drop the hook entirely and
-- register an app — ctrl+g opens a tiny overlay, enter compacts the
-- active session (ctx.session), esc closes.
--
--   local last = "press enter to compact this session"
--   rness.ui.app{
--     name = "compact",
--     slot = "overlay",
--     title = "compact",
--     keymap = "ctrl+g",
--     view = function(ctx) return { "  " .. last } end,
--     on_key = function(key, ctx)
--       if key == "enter" then
--         local done, report = pcall(rness.session.compact, ctx.session, KEEP_TURNS)
--         last = done and ("folded " .. report.shadowed .. " events")
--           or ("failed: " .. tostring(report))
--         return true
--       end
--       return false
--     end,
--   }
