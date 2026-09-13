-- User-only job inspection. These snapshots never consume the model's output
-- cursor, and commands remain available while the current turn is busy.
local usage = "Usage: /jobs [list | <job_id> | stop <job_id>]"

local function summary(job)
  local status = job.cancellation_requested and job.running and "stopping" or job.status
  if job.exit_code ~= nil then status = status .. " (code " .. job.exit_code .. ")" end
  return job.job_id .. " [" .. job.kind .. "] " .. status .. " — " .. job.label
end

rness.commands.register {
  name = "jobs",
  description = "List background jobs, inspect recent output, or request a stop",
  usage = "[list | <job_id> | stop <job_id>]",
  arguments = { "list", "stop" },
  allow_busy = true,
  complete = function(ctx)
    local choices = { "list", "stop" }
    for _, job in ipairs(rness.jobs.list(ctx.session)) do
      choices[#choices + 1] = job.job_id
      if job.running then choices[#choices + 1] = "stop " .. job.job_id end
    end
    return choices
  end,
  run = function(ctx)
    local input = ctx.raw_input:match("^%s*(.-)%s*$")
    if input == "" or input == "list" then
      local jobs = rness.jobs.list(ctx.session)
      if #jobs == 0 then return { message = "No background jobs in this session." } end
      local lines = { "Background jobs in this session:" }
      for _, job in ipairs(jobs) do lines[#lines + 1] = summary(job) end
      lines[#lines + 1] = "Inspect: /jobs <job_id>   Stop: /jobs stop <job_id>"
      return { message = table.concat(lines, "\n"), data = jobs }
    end
    local id = input:match("^stop%s+(%S+)$")
    if id then
      local requested = rness.jobs.stop(ctx.session, id)
      return { message = requested and ("Cancellation requested for job " .. id
        .. ". It remains active until its process settles.")
        or ("Job " .. id .. " is already stopped or stopping.") }
    end
    assert(not input:find("%s") and input ~= "stop", usage)
    local job = rness.jobs.inspect(ctx.session, input)
    local output = job.output ~= "" and job.output or "(no output yet)"
    local heading = "Recent output (non-consuming snapshot"
    if job.output_bytes and job.output_bytes > #job.output then
      heading = heading .. "; tail of " .. job.output_bytes .. " bytes"
    end
    return { message = summary(job) .. "\n" .. heading .. "):\n" .. output,
      data = job }
  end,
}
