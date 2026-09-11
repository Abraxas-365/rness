//! Explicit plugin source loading in startup declaration order.

use std::path::Path;

use crate::plugin_host::LuaHost;

/// An explicit startup selection. Inline callbacks remain in the startup VM.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    pub name: String,
    pub source: PluginLocation,
    pub enabled: bool,
    pub watch: bool,
    pub opts: serde_json::Value,
    pub keys: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PluginLocation {
    File(std::path::PathBuf),
    Package(String),
    Inline,
}

/// One discovered plugin source.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSource {
    /// Chunk name in Lua errors/tracebacks (e.g. "init.lua").
    pub name: String,
    pub source: String,
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
        return Err("plugin name must contain only ASCII letters, digits, '_' or '-' (no path or .lua suffix)".into());
    }
    Ok(())
}

/// Resolve explicit specifications without searching alternate source types.
pub fn discover_specs(root: &Path, specs: &[PluginSpec]) -> std::io::Result<Vec<PluginSource>> {
    let quote = |text: &str| format!("\"{}\"", text.as_bytes().iter().map(|b| format!("\\{b:03}")).collect::<String>());
    let mut result = Vec::new();
    for spec in specs.iter().filter(|spec| spec.enabled) {
        let chunk = match &spec.source {
            PluginLocation::File(path) => {
                let path = if path.is_absolute() { path.clone() } else { root.join(path) };
                let source = std::fs::read_to_string(&path).map_err(|error| std::io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
                format!("assert(load({}, {}, 't', _ENV))()", quote(&source), quote(&format!("@{}", path.display())))
            }
            PluginLocation::Package(name) => {
                let inventory = crate::packages::inventory(root)?;
                let package = inventory.get(name).ok_or_else(|| std::io::Error::other(format!("package {name} is not installed; install or link it before enabling")))?;
                if spec.watch && package.revision.is_some() { return Err(std::io::Error::other("watch is only supported for linked development packages")); }
                let sources = discover_package(root, name)?;
                format!("(function() {} end)()", sources.source)
            }
            PluginLocation::Inline => format!("__rness_plugin_callbacks[{}]", quote(&spec.name)),
        };
        let opts = serde_json::to_string(&spec.opts).map_err(std::io::Error::other)?;
        let keys = serde_json::to_string(&spec.keys).map_err(std::io::Error::other)?;
        let source = format!("local setup = {chunk}\nlocal opts = rness.json.decode({})\nlocal plugin = __rness_plugin_context(rness.json.decode({}))\nif type(setup) == 'function' then setup(opts, plugin) elseif setup ~= nil then error('plugin entrypoint must return a setup function or nil') elseif next(opts) ~= nil then error('plugin options require a setup function') end\nplugin.__finish()", quote(&opts), quote(&keys));
        result.push(PluginSource { name: spec.name.clone(), source });
    }
    Ok(result)
}

fn discover_package(root: &Path, name: &str) -> std::io::Result<PluginSource> {
    discover_selected(root, &[name.to_owned()], true)?.pop().ok_or_else(|| std::io::Error::other("missing package"))
}

/// Read only explicitly selected plugins, preserving declaration order.
/// Missing selected files are errors; unselected files are never read.
pub fn discover(root: &Path, names: &[String]) -> std::io::Result<Vec<PluginSource>> {
    discover_selected(root, names, false)
}

fn discover_selected(root: &Path, names: &[String], package_only: bool) -> std::io::Result<Vec<PluginSource>> {
    let installed = crate::packages::inventory(root)?;
    names.iter().map(|name| {
        validate_name(name).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        if let Some(package) = installed.get(name) {
            if !package_only && root.join(format!("plugins/{name}.lua")).exists() {
                return Err(std::io::Error::other(format!("ambiguous plugin name: {name}")));
            }
            let manifest = crate::packages::manifest(&package.directory)?;
            if manifest.name != *name { return Err(std::io::Error::other("linked package changed its name")); }
            let path = package.directory.join(manifest.entrypoint);
            let quote = |s: &str| -> String {
                format!("\"{}\"", s.as_bytes().iter().map(|b| format!("\\{b:03}")).collect::<String>())
            };
            let modules = package.directory.join("lua");
            let mut sources = std::collections::BTreeMap::new();
            collect_modules(&modules, &modules, &mut sources)?;
            let entries = sources.iter().map(|(name, source)| format!("[{}]={}", quote(name), quote(source))).collect::<Vec<_>>().join(",");
            let source = format!(r#"local sources = {{{entries}}}
local cache, loading = {{}}, {{}}
local fallback = require
local env = setmetatable({{}}, {{__index = _ENV, __newindex = _ENV}})
local function package_require(name)
    local source = sources[name]
    if source == nil then return fallback(name) end
    if cache[name] ~= nil then return cache[name] end
    if loading[name] then error('circular package require: ' .. name) end
    loading[name] = true
    local chunk, err = load(source, '@' .. name, 't', env)
    if not chunk then loading[name] = nil; error(err) end
    local ok, result = pcall(chunk, name)
    loading[name] = nil
    if not ok then error(result) end
    if result == nil then result = true end
    cache[name] = result
    return result
end
rawset(env, 'require', package_require)
local chunk, err = load({}, '@' .. {}, 't', env)
if not chunk then error(err) end
return chunk()
"#, quote(&std::fs::read_to_string(path)?), quote(name));
            return Ok(PluginSource { name: name.clone(), source });
        }
        let name = format!("plugins/{name}.lua");
        let path = root.join(&name);
        let source = std::fs::read_to_string(&path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
        })?;
        Ok(PluginSource { name, source })
    }).collect()
}

fn collect_modules(base: &Path, directory: &Path, sources: &mut std::collections::BTreeMap<String, String>) -> std::io::Result<()> {
    if !directory.exists() { return Ok(()); }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_symlink() { return Err(std::io::Error::other("package modules must not contain symlinks")); }
        if kind.is_dir() { collect_modules(base, &path, sources)?; }
        else if path.extension().is_some_and(|ext| ext == "lua") {
            let relative = path.strip_prefix(base).map_err(std::io::Error::other)?;
            let mut parts = relative.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>();
            if parts.last().is_some_and(|p| p == "init") { parts.pop(); }
            let name = parts.join(".");
            if name.is_empty() || sources.insert(name.clone(), std::fs::read_to_string(path)?).is_some() {
                return Err(std::io::Error::other(format!("ambiguous package module: {name}")));
            }
        }
    }
    Ok(())
}

/// Load discovered plugins into the VM. A broken plugin is reported and
/// skipped — one bad file doesn't take down the host.
pub async fn load_all(host: &LuaHost, sources: &[PluginSource]) -> Vec<(String, String)> {
    let mut errors = Vec::new();
    for p in sources {
        if let Err(e) = host.load(&p.name, &p.source).await {
            tracing::warn!(target: "lua", "plugin '{}' failed to load: {e}", p.name);
            errors.push((p.name.clone(), e));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_order_ignores_unselected_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("plugins")).unwrap();
        std::fs::write(root.join("init.lua"), "-- init").unwrap();
        std::fs::write(root.join("plugins/b.lua"), "-- b").unwrap();
        std::fs::write(root.join("plugins/a.lua"), "-- a").unwrap();
        std::fs::write(root.join("plugins/no.txt"), "not lua").unwrap();

        assert!(discover(root, &[]).unwrap().is_empty());
        let found = discover(root, &["b".into(), "a".into()]).unwrap();
        let names: Vec<_> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["plugins/b.lua", "plugins/a.lua"]);
    }

    #[test]
    fn missing_root_is_empty_not_error() {
        let root = Path::new("/nonexistent/rness");
        let found = discover(root, &[]).unwrap();
        assert!(discover(root, &["missing".into()]).is_err());
        for name in ["", "../escape", "a.lua", "/absolute", "a/b"] {
            assert!(validate_name(name).is_err());
        }
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn broken_plugin_is_skipped_others_load() {
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        let sources = vec![
            PluginSource { name: "bad.lua".into(), source: "not lua at all".into() },
            PluginSource {
                name: "good.lua".into(),
                source: r#"rness.tool.register{ name = "ok", run = function() return "si" end }"#
                    .into(),
            },
        ];
        let errors = load_all(&host, &sources).await;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "bad.lua");
        assert_eq!(host.tool_specs().await.len(), 1);
    }
}
