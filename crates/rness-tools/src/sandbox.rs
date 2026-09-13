//! OS process-sandbox launcher used by Bash. Lua chooses a mode; this module
//! either applies it below the shell or refuses to run. It never downgrades an
//! enforced mode to ordinary host execution.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use rness_protocol::sandbox::SandboxMode;

/// Immutable policy already resolved for a single session workspace.
#[derive(Debug, Clone)]
pub struct Policy {
    pub mode: SandboxMode,
    pub workspace: PathBuf,
}

impl Policy {
    pub fn new(mode: SandboxMode, workspace: &Path) -> Self {
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        Self { mode, workspace }
    }
}

/// Lifetime-owned private temporary area. Its path is passed only to the
/// confined child; removal happens after the process and descendants settle.
pub struct Lease {
    temp: Option<PathBuf>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(path) = self.temp.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn unavailable(mode: SandboxMode, detail: impl std::fmt::Display) -> String {
    format!(
        "sandbox mode '{mode:?}' is configured but no supported sandbox backend is usable on this host ({detail}); refusing to run the command unconfined"
    )
}

#[cfg(any(target_os = "macos", test))]
fn private_temp_in(parent: &Path) -> Result<Lease, String> {
    // TMPDIR is commonly a symlink on macOS. Seatbelt matches resolved paths.
    // Resolve before creating anything, and never adopt a pre-existing directory.
    let parent = std::fs::canonicalize(parent)
        .map_err(|e| format!("canonicalize sandbox temporary parent {}: {e}", parent.display()))?;
    let root = parent.join(format!("rness-sandbox-{}", ulid::Ulid::new()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&root)
        .map_err(|e| format!("create sandbox temporary directory {}: {e}", root.display()))?;
    // Own the path immediately, including on subsequent preparation/spawn errors.
    Ok(Lease { temp: Some(root) })
}

#[cfg(target_os = "macos")]
fn macos_profile(mode: SandboxMode) -> String {
    // Parameters keep arbitrary path bytes out of SBPL source. /dev/null needs
    // data writes, not the unlink/rename/metadata authority of file-write*.
    // This is a path boundary: pre-existing hardlinks in writable roots can
    // alias outside inodes. Workspaces must not contain such trusted-host aliases.
    let mut profile = String::from(
        "(version 1)\n(allow default)\n(deny file-write*)\n\
         (allow file-write-data (literal \"/dev/null\"))\n",
    );
    match mode {
        SandboxMode::ReadOnly => {}
        SandboxMode::WorkspaceWrite => profile.push_str(
            "(allow file-write* (subpath (param \"WORKSPACE\")) (subpath (param \"TEMP\")))\n",
        ),
        SandboxMode::DangerFullAccess => unreachable!("unconfined calls do not build a profile"),
    }
    profile
}

/// Add sandbox wrapping and private-temp environment to a shell command.
/// `sandbox-exec` inherits the existing process-group setup from the caller.
pub fn prepare(
    command: &str,
    workdir: &Path,
    policy: &Policy,
) -> Result<(tokio::process::Command, Lease), String> {
    if policy.mode == SandboxMode::DangerFullAccess {
        let mut process = tokio::process::Command::new("/bin/sh");
        process.arg("-c").arg(command);
        return Ok((process, Lease { temp: None }));
    }
    // Do not trust lexical containment (".." and symlinks can escape it).
    let workdir = std::fs::canonicalize(workdir)
        .map_err(|e| format!("canonicalize sandbox workdir {}: {e}", workdir.display()))?;
    let workspace = std::fs::canonicalize(&policy.workspace)
        .map_err(|e| format!("canonicalize sandbox workspace {}: {e}", policy.workspace.display()))?;
    if workspace != policy.workspace || !workspace.is_dir() {
        return Err("sandbox workspace is no longer a canonical directory; refusing to run".into());
    }
    if !workdir.starts_with(&workspace) {
        return Err(format!(
            "sandbox policy '{}' denies workdir {} outside session workspace {}",
            match policy.mode {
                SandboxMode::ReadOnly => "read-only",
                SandboxMode::WorkspaceWrite => "workspace-write",
                SandboxMode::DangerFullAccess => unreachable!(),
            },
            workdir.display(),
            policy.workspace.display()
        ));
    }
    #[cfg(target_os = "macos")]
    {
        prepare_macos(command, policy, Path::new("/usr/bin/sandbox-exec"), &std::env::temp_dir())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(unavailable(
            policy.mode,
            "no backend has been implemented for this platform",
        ))
    }
}

#[cfg(target_os = "macos")]
fn prepare_macos(
    command: &str,
    policy: &Policy,
    runner: &Path,
    temp_parent: &Path,
) -> Result<(tokio::process::Command, Lease), String> {
    if !runner.is_file() {
        return Err(unavailable(policy.mode, "sandbox-exec is unavailable"));
    }
    let lease = private_temp_in(temp_parent)?;
    let temp = lease.temp.as_ref().expect("private temporary directory");
    let mut workspace_param = std::ffi::OsString::from("WORKSPACE=");
    workspace_param.push(&policy.workspace);
    let mut temp_param = std::ffi::OsString::from("TEMP=");
    temp_param.push(temp);
    let mut process = tokio::process::Command::new(runner);
    process
        .arg("-D").arg(workspace_param)
        .arg("-D").arg(temp_param)
        .arg("-p")
        .arg(macos_profile(policy.mode))
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .env("TMPDIR", temp)
        .env("TMP", temp)
        .env("TEMP", temp);
    Ok((process, lease))
}

pub fn standard_io(process: &mut tokio::process::Command, workdir: &Path) {
    process
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfined_mode_needs_no_private_temp() {
        let dir = tempfile::tempdir().unwrap();
        let (_, lease) = prepare(
            "true",
            dir.path(),
            &Policy::new(SandboxMode::DangerFullAccess, dir.path()),
        )
        .unwrap();
        assert!(lease.temp.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_rejects_lexical_workdir_escape_and_retargeted_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let policy = Policy::new(SandboxMode::WorkspaceWrite, &workspace);
        for path in [workspace.join(".."), workspace.join("escape")] {
            if path.ends_with("escape") {
                std::os::unix::fs::symlink(root.path(), &path).unwrap();
            }
            let error = prepare("true", &path, &policy).err().unwrap();
            assert!(error.contains("outside session workspace"), "{error}");
        }
        std::fs::rename(&workspace, root.path().join("original")).unwrap();
        std::os::unix::fs::symlink(root.path(), &workspace).unwrap();
        let error = prepare("true", &workspace, &policy).err().unwrap();
        assert!(error.contains("no longer a canonical directory"), "{error}");
    }

    #[test]
    fn private_temp_is_owned_and_cleaned_on_error() {
        let parent = tempfile::tempdir().unwrap();
        let result = (|| -> Result<(), String> {
            let lease = private_temp_in(parent.path())?;
            let path = lease.temp.as_ref().unwrap();
            assert_eq!(*path, std::fs::canonicalize(path).unwrap());
            std::fs::write(path.join("payload"), "private").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
            }
            Err("subsequent preparation failed".into())
        })();
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn private_temp_resolves_symlinked_parent_and_does_not_follow_cleanup_links() {
        let parent = tempfile::tempdir().unwrap();
        let actual = parent.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        let alias = parent.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        let lease = private_temp_in(&alias).unwrap();
        let path = lease.temp.as_ref().unwrap().clone();
        assert_eq!(path.parent().unwrap(), std::fs::canonicalize(&actual).unwrap());
        let outside = parent.path().join("keep");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(parent.path(), path.join("escape")).unwrap();
        drop(lease);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_backend_fails_before_allocating_temp() {
        let parent = tempfile::tempdir().unwrap();
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            let error = prepare_macos(
                "touch should-not-run", &Policy::new(mode, parent.path()),
                &parent.path().join("missing-runner"), parent.path(),
            ).err().unwrap();
            assert!(error.contains("refusing to run"), "{error}");
        }
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn spawn_failure_releases_private_temp() {
        let parent = tempfile::tempdir().unwrap();
        let policy = Policy::new(SandboxMode::WorkspaceWrite, parent.path());
        let (mut process, lease) = prepare_macos(
            "true", &policy, Path::new("/usr/bin/sandbox-exec"), parent.path(),
        ).unwrap();
        let path = lease.temp.as_ref().unwrap().clone();
        process.current_dir(parent.path().join("missing-workdir"));
        assert!(process.spawn().is_err());
        drop(lease);
        assert!(!path.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_null_exception_only_allows_data_writes() {
        let profile = macos_profile(SandboxMode::ReadOnly);
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(allow file-write-data (literal \"/dev/null\"))"));
        assert!(!profile.contains("(allow file-write*"));
    }
}
