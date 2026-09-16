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
    /// Authorize a built-in file mutation. Reads remain unrestricted, matching
    /// the process policy. This is a path policy, not isolation from hostile
    /// concurrent filesystem changes or trusted plugin code.
    pub fn check_write(&self, path: &Path) -> Result<(), String> {
        match self.mode {
            SandboxMode::DangerFullAccess => return Ok(()),
            SandboxMode::ReadOnly => return Err("read-only sandbox denies file mutation".into()),
            SandboxMode::WorkspaceWrite => {}
        }
        let root = std::fs::canonicalize(&self.workspace).map_err(|e| format!("sandbox workspace: {e}"))?;
        if root != self.workspace || !root.is_dir() {
            return Err("sandbox workspace identity changed".into());
        }
        // Reject traversal rather than normalizing away symlink-sensitive '..'.
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err("workspace-write denies parent traversal in mutation paths".into());
        }
        let mut existing = path;
        loop {
            match std::fs::symlink_metadata(existing) {
                Ok(_) => {
                    let resolved = std::fs::canonicalize(existing)
                        .map_err(|e| format!("sandbox target: {e}"))?;
                    if !resolved.starts_with(&root) {
                        return Err("workspace-write denies mutation outside the session workspace".into());
                    }
                    // Resolve the final file too, so symlink aliases cannot hide hard links.
                    #[cfg(unix)]
                    if existing == path {
                        use std::os::unix::fs::MetadataExt;
                        if std::fs::metadata(existing).map_err(|e| e.to_string())?.nlink() > 1 && resolved.is_file() {
                            return Err("workspace-write denies mutation of multiply linked files".into());
                        }
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or("sandbox target has no existing ancestor")?;
                }
                Err(e) => return Err(format!("sandbox target: {e}")),
            }
        }
    }

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
    container: Option<(PathBuf, String)>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some((runner, name)) = self.container.take() {
            // Also runs when the attached docker client is cancelled. Killing
            // the client alone would otherwise leave the producer alive.
            let _ = std::process::Command::new(runner).args(["rm", "--force", &name])
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
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
    Ok(Lease { temp: Some(root), container: None })
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
    prepare_configured(command, workdir, policy, &Default::default())
}

pub fn prepare_configured(
    command: &str,
    workdir: &Path,
    policy: &Policy,
    config: &rness_engine::sandbox::ProcessConfig,
) -> Result<(tokio::process::Command, Lease), String> {
    if policy.mode == SandboxMode::DangerFullAccess {
        #[cfg(not(windows))]
        let process = {
            let mut process = tokio::process::Command::new(&config.unix_shell);
            process.arg("-c").arg(command);
            process
        };
        #[cfg(windows)]
        let process = {
            let mut process = tokio::process::Command::new(&config.windows_shell);
            process.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command]);
            process
        };
        return Ok((process, Lease { temp: None, container: None }));
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
        prepare_macos_configured(command, policy, &config.macos_runner, config.temp_parent.as_deref().unwrap_or(&std::env::temp_dir()), &config.unix_shell)
    }
    #[cfg(target_os = "linux")]
    {
        prepare_linux(command, &workdir, policy, config)
    }
    #[cfg(windows)]
    {
        prepare_windows_container(command, &workdir, policy, config)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Err(unavailable(
            policy.mode,
            "no backend has been implemented for this platform",
        ))
    }
}

#[cfg(all(target_os = "macos", test))]
fn prepare_macos(
    command: &str,
    policy: &Policy,
    runner: &Path,
    temp_parent: &Path,
) -> Result<(tokio::process::Command, Lease), String> {
    prepare_macos_configured(command, policy, runner, temp_parent, Path::new("/bin/sh"))
}

#[cfg(target_os = "macos")]
fn prepare_macos_configured(command: &str, policy: &Policy, runner: &Path, temp_parent: &Path, shell: &Path) -> Result<(tokio::process::Command, Lease), String> {
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
        .arg(shell)
        .arg("-c")
        .arg(command)
        .env("TMPDIR", temp)
        .env("TMP", temp)
        .env("TEMP", temp);
    Ok((process, lease))
}

#[cfg(any(target_os = "linux", test))]
fn prepare_linux(command: &str, workdir: &Path, policy: &Policy, config: &rness_engine::sandbox::ProcessConfig) -> Result<(tokio::process::Command, Lease), String> {
    if !config.linux_runner.is_file() { return Err(unavailable(policy.mode, "bubblewrap is unavailable")); }
    let mut process = tokio::process::Command::new(&config.linux_runner);
    process.args(["--die-with-parent", "--new-session", "--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev"]);
    process.args(["--remount-ro", "/dev"]);
    if policy.mode == SandboxMode::WorkspaceWrite {
        // Mount temporary storage first: workspaces under /tmp must not be
        // hidden by a later tmpfs mount.
        process.args(["--tmpfs", "/tmp"]);
        process.arg("--bind").arg(&policy.workspace).arg(&policy.workspace);
    }
    process.arg("--chdir").arg(workdir)
        .args(["--setenv", "TMPDIR", "/tmp", "--setenv", "TMP", "/tmp", "--setenv", "TEMP", "/tmp", "--"])
        .arg(&config.unix_shell).arg("-c").arg(command);
    Ok((process, Lease { temp: None, container: None }))
}

#[cfg(any(windows, test))]
fn prepare_windows_container(command: &str, workdir: &Path, policy: &Policy, config: &rness_engine::sandbox::ProcessConfig) -> Result<(tokio::process::Command, Lease), String> {
    let image = config.windows_container_image.as_deref().filter(|s| !s.is_empty() && !s.starts_with('-'))
        .ok_or_else(|| unavailable(policy.mode, "configure sandbox.process.windows_container_image and Docker Desktop Linux containers"))?;
    if config.windows_container_pids == 0 { return Err("windows_container_pids must be positive".into()); }
    let workspace = policy.workspace.to_str().ok_or("container workspace must be Unicode")?;
    let workspace = workspace.strip_prefix(r"\\?\").unwrap_or(workspace);
    if workspace.starts_with("UNC\\") { return Err("container workspace must be a local drive path".into()); }
    // Docker's --mount grammar has comma separators; reject rather than
    // reinterpret an arbitrary path as additional mount options.
    if workspace.contains(',') { return Err("container workspace cannot contain commas".into()); }
    let relative = workdir.strip_prefix(&policy.workspace).map_err(|_| "container workdir must be inside workspace")?;
    let name = format!("rness-{}", ulid::Ulid::new().to_string().to_lowercase());
    let mut process = tokio::process::Command::new(&config.windows_container_runner);
    process.args(["run", "--rm", "--pull=never", "--name", &name, "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit"])
        .arg(config.windows_container_pids.to_string());
    let mut mount = format!("type=bind,source={workspace},target=/workspace");
    if policy.mode == SandboxMode::ReadOnly { mount.push_str(",readonly"); }
    process.arg("--mount").arg(mount);
    if policy.mode == SandboxMode::WorkspaceWrite { process.args(["--tmpfs", "/tmp:rw,nosuid,nodev,noexec,size=64m"]); }
    process.arg("--workdir").arg(format!("/workspace/{}", relative.to_string_lossy().replace('\\', "/")))
        .arg("--entrypoint").arg(&config.windows_container_shell).arg(image).args(["-c", command]);
    Ok((process, Lease { temp: None, container: Some((config.windows_container_runner.clone(), name)) }))
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
    fn linux_launcher_uses_readonly_root_and_explicit_writable_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let runner = dir.path().join("bwrap");
        std::fs::write(&runner, b"").unwrap();
        let config = rness_engine::sandbox::ProcessConfig { linux_runner: runner, ..Default::default() };
        let error = prepare_windows_container("true", dir.path(), &Policy::new(SandboxMode::ReadOnly, dir.path()), &config).err().unwrap();
        assert!(error.contains("windows_container_image"));
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            let policy = Policy::new(mode, dir.path());
            let (process, _) = prepare_linux("true", dir.path(), &policy, &config).unwrap();
            let args: Vec<_> = process.as_std().get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            assert!(args.windows(3).any(|a| a == ["--ro-bind", "/", "/"]));
            assert_eq!(args.contains(&"--bind".into()), mode == SandboxMode::WorkspaceWrite);
            assert!(args.contains(&"--unshare-pid".into()));
        }
    }

    #[test]
    fn windows_container_launcher_limits_host_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy::new(SandboxMode::ReadOnly, dir.path());
        let config = rness_engine::sandbox::ProcessConfig {
            windows_container_image: Some("local-dev:latest".into()),
            ..Default::default()
        };
        let (process, mut lease) = prepare_windows_container("true", &policy.workspace, &policy, &config).unwrap();
        // Argument test only: do not invoke Docker during lease teardown.
        lease.container.take();
        let args: Vec<_> = process.as_std().get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"--read-only".into()));
        assert!(args.contains(&"--pull=never".into()));
        assert!(args.contains(&"--cap-drop=ALL".into()));
        assert_eq!(args.iter().filter(|a| *a == "--mount").count(), 1);
        assert!(args.iter().any(|a| a.ends_with("target=/workspace,readonly")));
        assert!(args.windows(2).any(|a| a == ["--workdir", "/workspace/"]));
    }

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
