# TUI commands and completion

Type `/` at the beginning of the input to open completion above the editor. Matching is by name prefix, not fuzzy search. Up/Down or Ctrl-N/Ctrl-P move the selection. Tab or Enter accepts a candidate without submitting it. Argument completion opens when candidates exist. Enter with the picker closed submits the input. Escape closes the picker without clearing the draft.

## Commands

| Input | Behavior |
| --- | --- |
| `/jobs`, `/jobs <job_id>` | With the default jobs plugin: open the live monitor or a job's non-consuming output, including retained finished jobs, even while busy. See [background jobs](../background-jobs.md). |
| `/jobs list`, `/jobs stop <job_id>` | Print active jobs or request cancellation of an explicit job. |
| `/terminals`, `/terminals <term_id>` | With the default terminals plugin: open the terminals monitor, or a terminal's live, non-consuming output, even while busy. See [persistent terminals](../terminals.md). |
| `/terminals list`, `/terminals stop <term_id>`, `/terminals close <term_id>` | Print this session's terminals, interrupt a terminal's running command (INT, then TERM/KILL; the shell stays), or close the terminal and everything started in it. |
| `/agents`, `/agents steer <id> <message>`, `/agents stop <id> [confirm]` | With agent controls: inspect descendants, steer, or request a confirmed stop. ID-first forms are also supported. See [subagent commands](../subagent-commands.md). |
| `/agent <name>` | Select a declared role in an idle session, persisting its configuration without a model turn. |
| `/skill <name> <input>` | Append skill instructions to the submitted content while preserving the original text. |
| `/<skill-name> <input>` | Shorthand for a discovered skill; use `/skill` when a name conflicts with a command. |
| `/unload <name>` | Unload a named plugin under engine maintenance. Does not uninstall it. |
| `/colorscheme <name>` | Change the local TUI palette immediately. Does not modify startup configuration. |
| `/title`, `/title <text>`, `/title auto`, `/title unpin` | With the default title plugin: show the session title (`(pinned)` when you set it), rename and pin it, regenerate it with the model now (also pins), or release a pin so automatic titles may replace it. Works while a turn runs; the title is committed at the next step boundary. See [session titles](#session-titles). |

## Session titles

Titles are pure Lua policy. The engine sets no title by itself and makes no title model calls. It only stores titles, enforces pins, and normalizes text. The default flavor's `title` plugin (`flavors/default/plugins/title.lua`) does the rest: `/title`, the fallback, the model call (prompt, profile, framing) and when to title. Edit it, replace it, or set `auto = "off"`.

With the default settings, each root session gets a fallback title as soon as its first human prompt is committed: the first five words, at most 64 bytes. A model title is then requested in the background with `rness.llm.complete` and replaces the fallback. Injected context, hook messages and job notices never count as prompts. Subagent sessions are not auto-titled.

```lua
rness.plugins.setup({
  -- …
  { name = "title", file = "plugins/title.lua", opts = {
      auto = "first",       -- "first": model title from the first prompt (dsh first-prompt)
                            -- "all":   retitle on every prompt, from all prompts so far (dsh all-prompts)
                            -- "off":   no automatic model titles (/title still works)
      fallback = true,      -- set the first-words title at once
      profile = "fast",     -- optional model profile; default: the session's model
      system = "…",         -- optional custom system prompt
      timeout = 60,         -- seconds before a request is abandoned
      max_bytes = 80,       -- cap on stored titles (≤ 200)
      max_input_bytes = 4096, max_output_tokens = 64,
  } },
})
rness.terminal_title = false -- default true: set the terminal window title (OSC)
```

In `all` mode, a newer prompt cancels a title request that is still in flight. When the framed prompts exceed `max_input_bytes`, the oldest prompts are dropped first.

The plugin uses only public APIs, so your own titler is a few lines. For example, a fixed-profile, custom-prompt titler:

```lua
rness.hook.on("prompt", function(ev)
  if ev.index ~= 1 or type(ev.parent) == "string" then return end
  rness.task.spawn(function()
    local ok, title = pcall(rness.llm.complete, {
      session = ev.session, profile = "fast", max_output_tokens = 32,
      system = "Name this coding session in 3 words.", prompt = ev.text,
    })
    if ok then rness.session.offer_title(ev.session, title, "model") end
  end)
end)
```

| API | Purpose |
| --- | --- |
| `rness.hook.on("prompt", fn)` | A human prompt was committed: `{ session, text, index, parent, lineage }`. `index` is 1-based within the session. `parent` is the parent session id for delegated sessions and JSON null (not `nil`) for roots, so test `type(ev.parent) == "string"`. |
| `rness.session.prompts(id)` | The session's human prompts, oldest first (own log; a fork does not inherit its parent's). |
| `rness.llm.complete{ session, prompt, system?, profile?, max_output_tokens?, timeout? }` | One bounded model call. It yields, so call it from a command, a tool or a [`rness.task`](../../reference/lua/task.md). |
| `rness.session.offer_title(id, text, "model"\|"fallback"[, max_bytes])` | Automatic title under the pin rules (`model` never replaces a pinned title; `fallback` only fills an untitled session). Returns whether it was accepted. |
| `rness.session.title(id[, text[, source]])`, `title_info(id)` | Get the title; set it explicitly (`user` pins, `model` unpins); `{ title, source }`. |

All titles — model, fallback, and user — are normalized to a single line with escape sequences, control characters, and bidirectional/invisible marks removed, so a title cannot inject terminal control codes. A title you set (`/title <text>`, `/title auto`, or `rness.session.title(id, text)`) is **pinned**: an in-flight automatic title that finishes later is discarded. `rness.session.title(id, text, "model")` or `/title unpin` stores an unpinned title. Title changes are durable `session/title` events and are broadcast as a `title_changed` frame (TUI, SSE) and the `session/title` bus event. The TUI shows the displayed session's title in the terminal window title as `<title> — rness`, clearing it on exit.

Agent and skill candidates are discovered when the TUI is constructed. Loaded-plugin candidates are refreshed from the VM roster by polling every 100 milliseconds; an already queued operation may delay the response. A disappearing candidate can still be typed manually, and execution checks the current state. The catalog is not a general Lua command-registration API.

## Skills and HTTP

Skills are discovered in project `.rness/skills` and user `~/.rness/skills`; the project wins name conflicts. Skill loading is repeated when a request is submitted. No `$ARGUMENTS` substitution occurs. The skill body and its resource directory are appended as another text part; the original input remains intact. Unknown explicit `/skill` requests fail. Unknown shorthand names remain ordinary text.

The CLI installs this resolver in `SessionService`, shared by TUI and HTTP sends. Custom service compositions must install their own input resolver to enable skill expansion. `/agent` is handled directly by the service for a request containing exactly one text part. For example, send this JSON to `POST /api/request` with a real session ID:

```json
{
  "type": "send",
  "session": "SESSION_ID",
  "intent": "followup",
  "content": [{ "kind": "text", "text": "/agent architect" }]
}
```

A successful role command returns the existing `log_only` disposition. It records a request-configuration event, not a user message. `/unload` and `/colorscheme` are local presentation/host commands, not HTTP slash commands.

## Errors and input history

The local backend acknowledges sends before displaying a user entry. Rejected sends produce notices rather than optimistic user messages. Successful role commands also produce a local notice. Asynchronous provider failures are separate from submission rejection.

Up/Down traverse input submitted during this TUI execution and restore the draft at the end. Consecutive identical submissions are deduplicated. Commands and unsuccessful submissions remain in input history. An open picker takes priority; multiline drafts retain vertical cursor movement. History is not persisted across restarts.

See [agents](../../reference/configuration/agents.md), [plugin lifecycle](../plugins/loading-and-lifecycle.md), and [colorschemes](colorschemes.md).
