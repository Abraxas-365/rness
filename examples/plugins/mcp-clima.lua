-- Add { name = "mcp-clima", file = "plugins/mcp-clima.lua" } to init.lua's
-- single rness.plugins.setup list after copying this file.
-- Ejemplo: conectar un server MCP desde un plugin (0 magia: esto ES la config)
--
-- Idempotente a proposito: el hot reload re-ejecuta el top level de cada
-- plugin, pero las conexiones MCP viven en el host y sobreviven al VM.
-- Reconectar seria un error ("already connected") — chequea primero.

for _, name in ipairs(rness.mcp.servers()) do
  if name == "clima" then return end -- ya conectado, reload no-op
end

-- El server de la demo es opcional: sin el script, el plugin es no-op
-- (un connect a un comando que muere seria un error ruidoso al cargar).
local f = io.open("/tmp/rness-mcp-fake.py", "r")
if not f then return end
f:close()

local tools = rness.mcp.connect{
  name = "clima",
  command = "python3",
  args = { "/tmp/rness-mcp-fake.py" },
  timeout_ms = 5000,
}
rness.log.info("mcp clima conectado: " .. tools[1])
