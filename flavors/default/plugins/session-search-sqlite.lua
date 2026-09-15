-- Opt-in provider only: no model tools, no disk access until the first search.
-- Add { name = "session-search-sqlite", file = "plugins/session-search-sqlite.lua" }
-- to your existing rness.plugins.setup list.
rness.session.register_search_provider(rness.session.sqlite_search_provider())
