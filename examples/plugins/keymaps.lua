-- keymaps.lua — rebind the HOST's keys (the Neovim model).
--
-- The engine ships a stock keymap (see rness-tui/src/keymaps.rs — the
-- ONE place the defaults live); this plugin rewrites entries at load
-- time and on every hot reload. Removing the plugin reverts everything:
-- the host recomputes stock+binds each sync.
--
-- Actions available today (host-side, fail-loud on typos):
--   scroll_up / scroll_down             one transcript line
--   scroll_up_page / scroll_down_page   ten lines
--   cancel_or_quit                      cancel a running turn, else quit
--   quit                                quit immediately
--
-- Chord language is shared with rness.ui.app{keymap=}: "ctrl+k",
-- "shift+up", "pageup", "alt+f2"…
--
-- NOTE: these are the keys the host handles AFTER the focused component
-- (editor, overlay) passes on a key. The editor's own bindings and app
-- toggle keys are separate seams.

-- vim-flavored scrolling without leaving the home row:
rness.keymaps.set("ctrl+k", "scroll_up")
rness.keymaps.set("ctrl+j", "scroll_down")

-- Disable a stock binding entirely (false = unbind):
-- rness.keymaps.set("ctrl+d", false)
