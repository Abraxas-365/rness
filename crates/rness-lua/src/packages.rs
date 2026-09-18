//! Data-only package installation. Activation remains an init.lua decision.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
};

fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub entrypoint: String,
    pub api_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub source: String,
    pub revision: Option<String>,
    pub directory: PathBuf,
}

pub fn inventory(root: &Path) -> io::Result<BTreeMap<String, Package>> {
    match fs::read(root.join("packages/lock.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(error),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e),
    }
}

pub fn manifest(directory: &Path) -> io::Result<Manifest> {
    let m: Manifest =
        serde_json::from_slice(&fs::read(directory.join("rness-plugin.json"))?).map_err(error)?;
    crate::loader::validate_name(&m.name).map_err(error)?;
    if m.api_version != 1 {
        return Err(error("unsupported plugin package API version"));
    }
    let entry = Path::new(&m.entrypoint);
    if entry.as_os_str().is_empty()
        || entry
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || entry.extension().is_none_or(|e| e != "lua")
    {
        return Err(error(
            "entrypoint must be a relative .lua path without traversal",
        ));
    }
    let base = directory.canonicalize()?;
    if !directory.join(entry).canonicalize()?.starts_with(&base) {
        return Err(error("entrypoint escapes package directory"));
    }
    fs::read_to_string(directory.join(entry))?;
    Ok(m)
}

fn git(directory: &Path, args: &[&str]) -> io::Result<String> {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.file.allow=never",
        ])
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !out.status.success() {
        return Err(error(String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Serialize writers and publish the inventory by atomic rename.
/// Superseded managed checkouts are removed only after publication.
pub fn change(
    root: &Path,
    source: Option<(&str, &str)>,
    local: Option<&Path>,
    target: Option<&str>,
) -> io::Result<String> {
    change_with(root, source, local, target, git, publish)
}

fn change_with(
    root: &Path,
    source: Option<(&str, &str)>,
    local: Option<&Path>,
    target: Option<&str>,
    run_git: impl Fn(&Path, &[&str]) -> io::Result<String>,
    publish: impl Fn(&Path, &BTreeMap<String, Package>) -> io::Result<()>,
) -> io::Result<String> {
    let packages = root.join("packages");
    fs::create_dir_all(&packages)?;
    let lock = packages.join(".writer");
    let _file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .map_err(|e| {
            error(format!(
                "package writer unavailable ({}): {e}",
                lock.display()
            ))
        })?;
    struct Unlock(PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _unlock = Unlock(lock);
    let mut installed = inventory(root)?;
    if source.is_none() && local.is_none() {
        let name = target.ok_or_else(|| error("missing package name"))?;
        let package = installed
            .remove(name)
            .ok_or_else(|| error("package is not installed"))?;
        if package.revision.is_some() {
            let base = packages.canonicalize()?;
            let directory = package.directory.canonicalize()?;
            if directory.parent() != Some(base.as_path()) || !directory.join(".git").is_dir() {
                return Err(error(
                    "refusing to remove a directory outside managed package storage",
                ));
            }
            // Rename first so failure to publish can restore the installation.
            let trash = tempfile::tempdir_in(&packages)?;
            let displaced = trash.path().join("removed");
            fs::rename(&directory, &displaced)?;
            if let Err(err) = publish(&packages, &installed) {
                if let Err(restore) = fs::rename(&displaced, &directory) {
                    let retained = trash.keep();
                    return Err(error(format!(
                        "{err}; rollback failed: {restore}; checkout retained at {}",
                        retained.join("removed").display()
                    )));
                }
                return Err(err);
            }
            trash.close()?;
        } else {
            publish(&packages, &installed)?;
        }
        return Ok(name.into());
    }
    let staging = tempfile::tempdir_in(&packages)?;
    let (directory, origin, revision) = if let Some(local) = local {
        let directory = local.canonicalize()?;
        (
            directory.clone(),
            directory.to_string_lossy().into_owned(),
            None,
        )
    } else {
        let (url, rev) = source.unwrap();
        if !url.starts_with("https://")
            || rev.is_empty()
            || rev.starts_with('-')
            || rev.chars().any(char::is_whitespace)
        {
            return Err(error(
                "install requires an HTTPS Git URL and an explicit revision",
            ));
        }
        run_git(staging.path(), &["init", "--template=", "."])?;
        run_git(
            staging.path(),
            &[
                "fetch",
                "--depth=1",
                "--no-recurse-submodules",
                "--",
                url,
                rev,
            ],
        )?;
        let commit = run_git(
            staging.path(),
            &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
        )?;
        // Reject symlinks/submodules before checkout; no repository build runs.
        let tree = run_git(staging.path(), &["ls-tree", "-r", &commit])?;
        if tree
            .lines()
            .any(|line| line.starts_with("120000 ") || line.starts_with("160000 "))
        {
            return Err(error("package symlinks and submodules are not supported"));
        }
        run_git(
            staging.path(),
            &[
                "-c",
                "filter.lfs.smudge=",
                "-c",
                "filter.lfs.required=false",
                "checkout",
                "--detach",
                &commit,
            ],
        )?;
        (staging.path().to_path_buf(), url.into(), Some(commit))
    };
    let m = manifest(&directory)?;
    if root.join(format!("plugins/{}.lua", m.name)).exists() {
        return Err(error("name conflicts with a local plugin"));
    }
    match target {
        Some(name) if name != m.name || !installed.contains_key(name) => {
            return Err(error("update must preserve an installed package name"))
        }
        None if installed.contains_key(&m.name) => {
            return Err(error("package already installed; use update"))
        }
        _ => {}
    }
    let previous = installed.insert(
        m.name.clone(),
        Package {
            source: origin,
            revision: revision.clone(),
            directory,
        },
    );
    let obsolete = previous
        .filter(|p| p.revision.is_some())
        .map(|p| {
            let directory = p.directory.canonicalize()?;
            if directory.parent() != Some(packages.canonicalize()?.as_path())
                || !directory.join(".git").is_dir()
            {
                return Err(error(
                    "previous checkout is outside managed package storage",
                ));
            }
            Ok(directory)
        })
        .transpose()?;
    publish(&packages, &installed)?;
    if revision.is_some() {
        let _ = staging.keep();
    }
    if let Some(directory) = obsolete {
        fs::remove_dir_all(&directory).map_err(|e| {
            error(format!(
                "update committed, but old checkout cleanup failed at {}: {e}",
                directory.display()
            ))
        })?;
    }
    Ok(m.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_install_update_and_transaction_failures() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "--template=", "."]).unwrap();
        fs::write(
            repo.path().join("rness-plugin.json"),
            r#"{"name":"sample","entrypoint":"plugin.lua","api_version":1}"#,
        )
        .unwrap();
        fs::write(repo.path().join("plugin.lua"), "error('must not execute')").unwrap();
        git(repo.path(), &["add", "."]).unwrap();
        git(
            repo.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "one",
            ],
        )
        .unwrap();
        let first = git(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        // Only substitute the transport endpoint; fetch/checkout use real Git.
        let transport = |directory: &Path, args: &[&str]| {
            if args.first() == Some(&"fetch") {
                git(
                    directory,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "fetch",
                        "--depth=1",
                        "--no-recurse-submodules",
                        "--",
                        repo.path().to_str().unwrap(),
                        args.last().unwrap(),
                    ],
                )
            } else {
                git(directory, args)
            }
        };
        let url = "https://example.com/plugin";
        change_with(
            root.path(),
            Some((url, &first)),
            None,
            None,
            transport,
            publish,
        )
        .unwrap();
        let original = inventory(root.path()).unwrap()["sample"].clone();
        assert_eq!(original.revision.as_deref(), Some(first.as_str()));
        fs::write(repo.path().join("plugin.lua"), "-- second").unwrap();
        git(repo.path(), &["add", "."]).unwrap();
        git(
            repo.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "two",
            ],
        )
        .unwrap();
        let second = git(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        let fail =
            |_: &Path, _: &BTreeMap<String, Package>| Err(error("injected publication failure"));
        assert!(change_with(
            root.path(),
            Some((url, &second)),
            None,
            Some("sample"),
            transport,
            fail
        )
        .is_err());
        assert_eq!(
            inventory(root.path()).unwrap()["sample"].directory,
            original.directory
        );
        assert!(original.directory.exists());
        assert_eq!(
            fs::read_dir(root.path().join("packages")).unwrap().count(),
            2
        );
        assert!(change_with(root.path(), None, None, Some("sample"), transport, fail).is_err());
        assert!(original.directory.join("plugin.lua").exists());
        assert!(change_with(
            root.path(),
            Some((url, "missing-ref")),
            None,
            Some("sample"),
            transport,
            publish
        )
        .is_err());
        change_with(
            root.path(),
            Some((url, &second)),
            None,
            Some("sample"),
            transport,
            publish,
        )
        .unwrap();
        assert!(!original.directory.exists());
        let updated = inventory(root.path()).unwrap()["sample"].clone();
        assert_eq!(updated.revision.as_deref(), Some(second.as_str()));
        change(root.path(), None, None, Some("sample")).unwrap();
        assert!(!updated.directory.exists());
        assert!(!root.path().join("packages/.writer").exists());
    }

    #[tokio::test]
    async fn link_is_data_only_and_activation_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(
            source.path().join("rness-plugin.json"),
            r#"{"name":"sample","entrypoint":"plugin.lua","api_version":1}"#,
        )
        .unwrap();
        fs::create_dir_all(source.path().join("lua")).unwrap();
        fs::write(source.path().join("lua/helper.lua"), "return {value='ok'}").unwrap();
        fs::write(
            source.path().join("plugin.lua"),
            "rness.tool.register{name='sample', run=function() return require('helper').value end}",
        )
        .unwrap();
        assert_eq!(
            change(root.path(), None, Some(source.path()), None).unwrap(),
            "sample"
        );
        assert!(change(root.path(), None, Some(source.path()), None).is_err());
        assert!(crate::loader::discover(root.path(), &[])
            .unwrap()
            .is_empty());
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        let selected = crate::loader::discover(root.path(), &["sample".into()]).unwrap();
        assert!(crate::loader::load_all(&host, &selected).await.is_empty());
        assert_eq!(
            host.call_tool("sample", serde_json::json!({}))
                .await
                .unwrap(),
            "ok"
        );
        change(root.path(), None, None, Some("sample")).unwrap();
        assert!(inventory(root.path()).unwrap().is_empty());
        assert!(source.path().join("plugin.lua").exists());
    }

    #[test]
    fn managed_remove_deletes_checkout_but_rejects_outside_paths() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("packages");
        fs::create_dir_all(&storage).unwrap();
        let checkout = storage.join("checkout");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        let mut entries = BTreeMap::new();
        entries.insert(
            "sample".into(),
            Package {
                source: "https://example.com/plugin".into(),
                revision: Some("commit".into()),
                directory: checkout.clone(),
            },
        );
        publish(&storage, &entries).unwrap();
        change(root.path(), None, None, Some("sample")).unwrap();
        assert!(!checkout.exists());
        assert!(inventory(root.path()).unwrap().is_empty());
        let outside = tempfile::tempdir().unwrap();
        entries.get_mut("sample").unwrap().directory = outside.path().to_owned();
        publish(&storage, &entries).unwrap();
        assert!(change(root.path(), None, None, Some("sample")).is_err());
        assert!(outside.path().exists());
        assert!(inventory(root.path()).unwrap().contains_key("sample"));
    }

    #[test]
    fn invalid_manifest_does_not_publish_and_releases_writer() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(
            source.path().join("rness-plugin.json"),
            r#"{"name":"sample","entrypoint":"../outside.lua","api_version":1}"#,
        )
        .unwrap();
        assert!(change(root.path(), None, Some(source.path()), None).is_err());
        assert!(inventory(root.path()).unwrap().is_empty());
        assert!(!root.path().join("packages/.writer").exists());
        assert!(change(root.path(), Some(("file:///tmp/repo", "main")), None, None).is_err());
    }
}

fn publish(directory: &Path, packages: &BTreeMap<String, Package>) -> io::Result<()> {
    use io::Write;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec_pretty(packages).map_err(error)?)?;
    file.as_file().sync_all()?;
    file.persist(directory.join("lock.json")).map_err(error)?;
    Ok(())
}
