//! Explicit plugin source loading in stable dependency-first order.

use std::path::Path;

use crate::plugin_host::LuaHost;

/// An explicit startup selection. Inline callbacks remain in the startup VM.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    pub name: String,
    pub source: PluginLocation,
    #[serde(default)]
    pub dependencies: Vec<String>,
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
    pub dependencies: Vec<String>,
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
        return Err("plugin name must contain only ASCII letters, digits, '_' or '-' (no path or .lua suffix)".into());
    }
    Ok(())
}

/// Validate declarations and return enabled indices in stable DFS dependency-first order.
/// Roots and dependency edges retain declaration order. Disabled specs still have
/// their names and dependency lists validated, but do not participate in the graph.
pub fn dependency_order(specs: &[PluginSpec]) -> Result<Vec<usize>, String> {
    for spec in specs {
        validate_name(&spec.name).map_err(|error| format!("plugin {}: {error}", spec.name))?;
        for dependency in &spec.dependencies {
            validate_name(dependency).map_err(|error| {
                format!("plugin {} dependency {dependency}: {error}", spec.name)
            })?;
        }
    }
    graph_order(
        &specs
            .iter()
            .map(|spec| {
                (
                    spec.name.as_str(),
                    spec.dependencies.as_slice(),
                    spec.enabled,
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// Validate and order already-discovered sources, including legacy chunk names.
pub fn ordered_sources(sources: &[PluginSource]) -> Result<Vec<&PluginSource>, String> {
    let order = graph_order(
        &sources
            .iter()
            .map(|source| (source.name.as_str(), source.dependencies.as_slice(), true))
            .collect::<Vec<_>>(),
    )?;
    Ok(order.into_iter().map(|index| &sources[index]).collect())
}

fn graph_order(nodes: &[(&str, &[String], bool)]) -> Result<Vec<usize>, String> {
    let mut names = std::collections::HashMap::new();
    for (index, (name, dependencies, _)) in nodes.iter().enumerate() {
        if names.insert(*name, index).is_some() {
            return Err(format!("duplicate plugin: {name}"));
        }
        let mut seen = std::collections::HashSet::new();
        for dependency in *dependencies {
            if dependency == name {
                return Err(format!("plugin {name} depends on itself"));
            }
            if !seen.insert(dependency) {
                return Err(format!(
                    "plugin {name} has duplicate dependency: {dependency}"
                ));
            }
        }
    }
    let mut edges = vec![Vec::new(); nodes.len()];
    for (index, (name, dependencies, enabled)) in nodes.iter().enumerate() {
        if !enabled {
            continue;
        }
        for dependency in *dependencies {
            let target = *names
                .get(dependency.as_str())
                .ok_or_else(|| format!("plugin {name} depends on missing plugin: {dependency}"))?;
            if !nodes[target].2 {
                return Err(format!(
                    "plugin {name} depends on disabled plugin: {dependency}"
                ));
            }
            edges[index].push(target);
        }
    }
    fn visit(
        index: usize,
        nodes: &[(&str, &[String], bool)],
        edges: &[Vec<usize>],
        states: &mut [u8],
        stack: &mut Vec<usize>,
        order: &mut Vec<usize>,
    ) -> Result<(), String> {
        if states[index] == 2 {
            return Ok(());
        }
        if states[index] == 1 {
            let start = stack.iter().position(|&item| item == index).unwrap();
            let mut cycle = stack[start..]
                .iter()
                .map(|&item| nodes[item].0)
                .collect::<Vec<_>>();
            cycle.push(nodes[index].0);
            return Err(format!("plugin dependency cycle: {}", cycle.join(" -> ")));
        }
        states[index] = 1;
        stack.push(index);
        for &dependency in &edges[index] {
            visit(dependency, nodes, edges, states, stack, order)?;
        }
        stack.pop();
        states[index] = 2;
        order.push(index);
        Ok(())
    }
    let mut states = vec![0; nodes.len()];
    let mut stack = Vec::new();
    let mut order = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if node.2 {
            visit(index, nodes, &edges, &mut states, &mut stack, &mut order)?;
        }
    }
    Ok(order)
}

/// Resolve explicit specifications without searching alternate source types.
pub fn discover_specs(root: &Path, specs: &[PluginSpec]) -> std::io::Result<Vec<PluginSource>> {
    let quote = |text: &str| format!("\"{}\"", text.as_bytes().iter().map(|b| format!("\\{b:03}")).collect::<String>());
    let mut result = Vec::new();
    let order = dependency_order(specs).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    for index in order {
        let spec = &specs[index];
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
        result.push(PluginSource { name: spec.name.clone(), source, dependencies: spec.dependencies.clone() });
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
            return Ok(PluginSource { name: name.clone(), source, dependencies: Vec::new() });
        }
        let name = format!("plugins/{name}.lua");
        let path = root.join(&name);
        let source = std::fs::read_to_string(&path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
        })?;
        Ok(PluginSource { name, source, dependencies: Vec::new() })
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

/// Validate and dependency-sort sources before loading any into the VM.
/// Graph errors are returned under the name "plugins". Individual load failures
/// are reported and skipped; unrelated plugins can still load.
pub async fn load_all(host: &LuaHost, sources: &[PluginSource]) -> Vec<(String, String)> {
    let sources = match ordered_sources(sources) {
        Ok(sources) => sources,
        Err(error) => return vec![("plugins".into(), error)],
    };
    let mut errors = Vec::new();
    let mut failed = std::collections::HashSet::new();
    for p in sources {
        let result = if let Some(dependency) = p.dependencies.iter().find(|name| failed.contains(name.as_str())) {
            Err(format!("dependency {dependency} failed or was skipped in this batch"))
        } else {
            host.load_with_dependencies(&p.name, &p.source, &p.dependencies).await
        };
        if let Err(e) = result {
            failed.insert(p.name.as_str());
            tracing::warn!(target: "lua", "plugin '{}' failed to load: {e}", p.name);
            errors.push((p.name.clone(), e));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, dependencies: &[&str]) -> PluginSpec {
        PluginSpec {
            name: name.into(),
            source: PluginLocation::Inline,
            dependencies: dependencies.iter().map(|name| (*name).into()).collect(),
            enabled: true,
            watch: false,
            opts: serde_json::json!({}),
            keys: serde_json::json!({}),
        }
    }

    #[test]
    fn dependencies_default_when_deserializing_old_specs() {
        let mut value = serde_json::to_value(spec("old", &[])).unwrap();
        value.as_object_mut().unwrap().remove("dependencies");
        let parsed: PluginSpec = serde_json::from_value(value).unwrap();
        assert!(parsed.dependencies.is_empty());
    }

    #[test]
    fn dependency_order_is_stable_and_shared_dependencies_load_once() {
        let specs = vec![
            spec("app", &["right", "left"]),
            spec("unrelated", &[]),
            spec("left", &["base"]),
            spec("right", &["base"]),
            spec("base", &[]),
        ];
        assert_eq!(dependency_order(&specs).unwrap(), [4, 3, 2, 0, 1]);
        let root = tempfile::tempdir().unwrap();
        let sources = discover_specs(root.path(), &specs).unwrap();
        assert_eq!(
            sources
                .iter()
                .map(|source| source.name.as_str())
                .collect::<Vec<_>>(),
            ["base", "right", "left", "app", "unrelated"]
        );
        assert_eq!(sources[3].dependencies, ["right", "left"]);
        let unordered = vec![
            sources[3].clone(),
            sources[4].clone(),
            sources[2].clone(),
            sources[1].clone(),
            sources[0].clone(),
        ];
        assert_eq!(
            ordered_sources(&unordered).unwrap(),
            sources.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn dependency_graph_errors_precede_source_reads() {
        for (specs, expected) in [
            (vec![spec("bad.lua", &[])], "plugin name"),
            (vec![spec("app", &["bad.lua"])], "dependency bad.lua"),
            (vec![spec("app", &[]), spec("app", &[])], "duplicate plugin"),
            (
                vec![spec("app", &["base", "base"]), spec("base", &[])],
                "duplicate dependency",
            ),
            (vec![spec("app", &["app"])], "depends on itself"),
            (vec![spec("app", &["missing"])], "missing plugin"),
            (
                vec![
                    spec("app", &["base"]),
                    PluginSpec {
                        enabled: false,
                        ..spec("base", &[])
                    },
                ],
                "disabled plugin",
            ),
            (
                vec![
                    spec("app", &["base"]),
                    spec("base", &["third"]),
                    spec("third", &["app"]),
                ],
                "app -> base -> third -> app",
            ),
            (
                vec![spec("unrelated", &[]), spec("a", &["b"]), spec("b", &["a"])],
                "a -> b -> a",
            ),
        ] {
            let error = dependency_order(&specs).unwrap_err();
            assert!(error.contains(expected), "{error}");
            let specs = specs
                .into_iter()
                .map(|spec| PluginSpec {
                    source: PluginLocation::File("missing.lua".into()),
                    ..spec
                })
                .collect::<Vec<_>>();
            let error = discover_specs(Path::new("/nonexistent/rness"), &specs).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn disabled_specs_skip_target_checks_but_validate_dependency_lists() {
        let mut disabled = spec("disabled", &["missing"]);
        disabled.enabled = false;
        assert!(dependency_order(&[disabled.clone()]).unwrap().is_empty());
        disabled.dependencies = vec!["disabled".into()];
        assert!(dependency_order(&[disabled.clone()]).is_err());
        disabled.dependencies = vec!["bad.lua".into()];
        assert!(dependency_order(&[disabled]).is_err());
    }

    #[test]
    fn source_order_accepts_legacy_names_and_rejects_invalid_graphs() {
        let legacy = PluginSource {
            name: "plugins/old.lua".into(),
            source: String::new(),
            dependencies: Vec::new(),
        };
        assert_eq!(
            ordered_sources(std::slice::from_ref(&legacy)).unwrap(),
            vec![&legacy]
        );
        assert!(
            ordered_sources(&[legacy.clone(), legacy.clone()])
                .unwrap_err()
                .contains("duplicate plugin")
        );
        let mut source = legacy;
        source.dependencies.push("missing".into());
        assert!(
            ordered_sources(std::slice::from_ref(&source))
                .unwrap_err()
                .contains("missing plugin")
        );
        source.dependencies = vec![source.name.clone()];
        assert!(ordered_sources(&[source]).unwrap_err().contains("itself"));
    }

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
        assert!(found.iter().all(|source| source.dependencies.is_empty()));
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
    async fn direct_sources_are_sorted_and_invalid_graphs_load_nothing() {
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        let app = PluginSource {
            name: "app".into(),
            source: "assert(base_ran); app_ran = true".into(),
            dependencies: vec!["base".into()],
        };
        let base = PluginSource {
            name: "base".into(),
            source: "base_ran = true".into(),
            dependencies: Vec::new(),
        };
        let invalid = PluginSource {
            name: "invalid".into(),
            source: String::new(),
            dependencies: vec!["missing".into()],
        };
        let errors = load_all(&host, &[base.clone(), invalid]).await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("missing plugin"));
        host.load("verify_empty", "assert(base_ran == nil)")
            .await
            .unwrap();
        assert!(load_all(&host, &[app, base]).await.is_empty());
        host.load("verify_loaded", "assert(app_ran)").await.unwrap();
    }

    #[tokio::test]
    async fn failed_dependency_skips_dependents_but_not_unrelated_plugins() {
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        let sources = vec![
            PluginSource {
                name: "base".into(),
                source: "error('broken')".into(),
                dependencies: Vec::new(),
            },
            PluginSource {
                name: "app".into(),
                source: "dependent_ran = true".into(),
                dependencies: vec!["base".into()],
            },
            PluginSource {
                name: "leaf".into(),
                source: "dependent_ran = true".into(),
                dependencies: vec!["app".into()],
            },
            PluginSource {
                name: "unrelated".into(),
                source: "unrelated_ran = true".into(),
                dependencies: Vec::new(),
            },
        ];
        let errors = load_all(&host, &sources).await;
        assert_eq!(
            errors
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["base", "app", "leaf"]
        );
        host.load(
            "verify",
            "assert(dependent_ran == nil); assert(unrelated_ran)",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_batch_dependency_does_not_fall_back_to_preloaded_plugin() {
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        host.load("base", "base_version = 'old'").await.unwrap();
        let sources = vec![
            PluginSource {
                name: "base".into(),
                source: "base_version = 'new'".into(),
                dependencies: Vec::new(),
            },
            PluginSource {
                name: "app".into(),
                source: "app_ran = true".into(),
                dependencies: vec!["base".into()],
            },
            PluginSource {
                name: "leaf".into(),
                source: "leaf_ran = true".into(),
                dependencies: vec!["app".into()],
            },
            PluginSource {
                name: "unrelated".into(),
                source: "unrelated_ran = true".into(),
                dependencies: Vec::new(),
            },
        ];
        let errors = load_all(&host, &sources).await;
        assert_eq!(
            errors.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["base", "app", "leaf"]
        );
        assert!(errors[1].1.contains("dependency base failed or was skipped in this batch"));
        assert!(errors[2].1.contains("dependency app failed or was skipped in this batch"));
        host.load(
            "verify",
            "assert(base_version == 'old'); assert(app_ran == nil); assert(leaf_ran == nil); assert(unrelated_ran)",
        ).await.unwrap();
    }

    #[tokio::test]
    async fn broken_plugin_is_skipped_others_load() {
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        let sources = vec![
            PluginSource { name: "bad.lua".into(), source: "not lua at all".into(), dependencies: Vec::new() },
            PluginSource {
                name: "good.lua".into(),
                dependencies: Vec::new(),
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
