//! rness.http — blocking HTTP from the VM actor thread.
//!
//! Blocking is correct here: the VM runs on its own dedicated thread
//! (see plugin_host), so a request stalls Lua work only — the engine
//! and TUI never wait on it. Timeout is mandatory (default 30s).
//!
//! rness.http.request{ url=, method=, headers=, body=, timeout_ms= }
//!   -> { status = 200, body = "...", headers = { [k] = v } }
//! rness.http.get(url) — shorthand.

use mlua::{Lua, Table};
use std::time::Duration;

fn do_request(lua: &Lua, spec: Table) -> Result<Table, mlua::Error> {
    let url: String = spec
        .get("url")
        .map_err(|_| mlua::Error::runtime("rness.http.request: 'url' (string) is required"))?;
    let method: String = spec.get::<Option<String>>("method")?.unwrap_or_else(|| "GET".into());
    let timeout_ms: u64 = spec.get::<Option<u64>>("timeout_ms")?.unwrap_or(30_000);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| mlua::Error::runtime(format!("http client: {e}")))?;

    let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
        .map_err(|_| mlua::Error::runtime(format!("http: bad method '{method}'")))?;
    let mut req = client.request(m, &url);

    if let Some(headers) = spec.get::<Option<Table>>("headers")? {
        for pair in headers.pairs::<String, String>() {
            let (k, v) = pair?;
            req = req.header(k, v);
        }
    }
    if let Some(body) = spec.get::<Option<String>>("body")? {
        req = req.body(body);
    }

    let resp = req
        .send()
        .map_err(|e| mlua::Error::runtime(format!("http {method} {url}: {e}")))?;

    let out = lua.create_table()?;
    out.set("status", resp.status().as_u16())?;
    let headers = lua.create_table()?;
    for (k, v) in resp.headers() {
        headers.set(k.as_str(), v.to_str().unwrap_or_default())?;
    }
    out.set("headers", headers)?;
    out.set(
        "body",
        resp.text()
            .map_err(|e| mlua::Error::runtime(format!("http {method} {url}: body: {e}")))?,
    )?;
    Ok(out)
}

pub fn install(lua: &Lua, rness: &Table) -> Result<(), mlua::Error> {
    let http = lua.create_table()?;

    http.set(
        "request",
        lua.create_function(|lua, spec: Table| do_request(lua, spec))?,
    )?;

    http.set(
        "get",
        lua.create_function(|lua, url: String| {
            let spec = lua.create_table()?;
            spec.set("url", url)?;
            do_request(lua, spec)
        })?,
    )?;

    rness.set("http", http)?;
    Ok(())
}
