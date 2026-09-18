//! HTTP requests on the Lua actor, with cancellation covering headers and body.
use mlua::{Lua, Table};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn do_request(lua: &Lua, spec: Table) -> Result<Table, mlua::Error> {
    let url: String = spec.get("url")?;
    let method: String = spec
        .get::<Option<String>>("method")?
        .unwrap_or_else(|| "GET".into());
    let timeout_ms = spec.get::<Option<u64>>("timeout_ms")?.unwrap_or(30_000);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(mlua::Error::external)?;
    let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
        .map_err(mlua::Error::external)?;
    let mut request = client.request(method, url);
    if let Some(headers) = spec.get::<Option<Table>>("headers")? {
        for pair in headers.pairs::<String, String>() {
            let (key, value) = pair?;
            request = request.header(key, value);
        }
    }
    if let Some(body) = spec.get::<Option<String>>("body")? {
        request = request.body(body);
    }
    let cancel = lua
        .app_data_ref::<CancellationToken>()
        .map(|token| token.clone())
        .unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(mlua::Error::external)?;
    let (status, headers, body) = runtime.block_on(async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(mlua::Error::runtime("command cancelled")),
            result = async move {
                let response = request.send().await.map_err(mlua::Error::external)?;
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let body = response.text().await.map_err(mlua::Error::external)?;
                Ok::<_, mlua::Error>((status, headers, body))
            } => result,
        }
    })?;
    let out = lua.create_table()?;
    out.set("status", status)?;
    let values = lua.create_table()?;
    for (key, value) in &headers {
        values.set(key.as_str(), value.to_str().unwrap_or_default())?;
    }
    out.set("headers", values)?;
    out.set("body", body)?;
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
