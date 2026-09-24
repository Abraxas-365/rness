//! `rness.llm` — profile-aware LLM completion for plugins.
//!
//! ```lua
//! local text = rness.llm.complete {
//!   prompt   = "Help me debug ...",        -- required
//!   system   = "You are a ...",            -- optional system prompt
//!   session  = ctx.session,                -- optional; auto-detected in commands
//!   profile  = "title-gen",                -- optional; defaults to session model
//!   timeout  = 15,                         -- seconds; default 30
//! }
//! ```

use std::sync::Arc;

use mlua::{Lua, Table};

use crate::api::session::{CommandYield, command_yield_wrapper};

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

pub fn install(
    lua: &Lua,
    rness: &Table,
    sessions: Arc<rness_engine::service::SessionService>,
) -> Result<(), mlua::Error> {
    let llm = lua.create_table()?;

    let svc = sessions;
    let prepare = lua.create_function(move |lua, opts: Table| {
        let system: String = opts.get::<Option<String>>("system")?.unwrap_or_default();
        let prompt: String = opts
            .get("prompt")
            .map_err(|_| err("rness.llm.complete: 'prompt' is required"))?;
        let profile: Option<String> = opts.get("profile")?;
        let timeout_secs: Option<u64> = opts.get("timeout")?;
        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(30));

        // Session resolution: explicit > __rness_callback_owner (commands) > error.
        let session_id: String = opts
            .get::<Option<String>>("session")?
            .or_else(|| {
                lua.globals()
                    .get::<Option<String>>("__rness_callback_owner")
                    .ok()
                    .flatten()
            })
            .ok_or_else(|| {
                err(
                    "rness.llm.complete: 'session' is required \
                     (pass explicitly from tool context, e.g. session = ctx.session)",
                )
            })?;

        let cancel = lua
            .app_data_ref::<tokio_util::sync::CancellationToken>()
            .map(|c| c.clone())
            .unwrap_or_default();

        let svc = svc.clone();
        Ok(CommandYield(Box::pin(async move {
            let result = svc
                .llm_complete(
                    &session_id,
                    &system,
                    &prompt,
                    profile.as_deref(),
                    timeout,
                    cancel,
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::Value::String(result))
        })))
    })?;

    llm.set("complete", command_yield_wrapper(lua, prepare)?)?;
    rness.set("llm", llm)?;
    Ok(())
}
