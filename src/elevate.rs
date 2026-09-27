//! Running the write with the privileges raw disk access needs.
//!
//! Windows: the executable carries a `requireAdministrator` manifest, so the whole program already
//! runs elevated. macOS: the disk is opened through the system `authopen` helper (see
//! `blockdev`), the program itself stays unprivileged. Linux: the GUI launches a second copy of
//! itself as root through `pkexec` (or `sudo -A`); that helper writes JSON progress lines to a
//! file the GUI polls, and stops when a `.cancel` file appears next to it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};

/// True when card writes must go through a separate root helper process. Windows runs
/// elevated from the start and macOS gets the disk descriptor from `authopen`, so only Linux
/// without root needs the helper.
pub fn needs_helper() -> bool {
    cfg!(target_os = "linux") && !is_privileged()
}

/// Creates a private directory for the progress and cancel files of one job. A file created by
/// the user directly in the sticky /tmp cannot be opened by the root helper on Linux
/// (fs.protected_regular), so the files live in a directory of their own.
pub fn job_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!(
        "rockchip_sd_tool_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

pub fn is_privileged() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Arguments of the helper invocation for a job.
pub fn helper_args(spec: &crate::job::JobSpec, progress_file: &Path) -> Vec<String> {
    let mut a = vec![
        "write".to_string(),
        spec.image.to_string_lossy().to_string(),
        "--to".to_string(),
        spec.output.clone(),
        "--yes".to_string(),
        "--progress-file".to_string(),
        progress_file.to_string_lossy().to_string(),
    ];
    if let Some(s) = spec.size {
        a.push("--size".into());
        a.push(format!("{s}"));
    }
    if !spec.verify {
        a.push("--no-verify".into());
    }
    if !spec.verify_blocks {
        a.push("--no-block-verify".into());
    }
    a
}

/// Launches the privileged helper. Returns the child process to wait on.
pub fn spawn_helper(spec: &crate::job::JobSpec, progress_file: &Path) -> Result<Child> {
    let exe = std::env::current_exe().context("cannot find my own executable")?;
    let args = helper_args(spec, progress_file);
    if is_privileged() {
        return Command::new(&exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start the write helper");
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(p) = which("pkexec") {
            let child = Command::new(p)
                .arg(&exe)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .context("cannot start pkexec")?;
            return Ok(child);
        }
        if let Ok(p) = which("sudo") {
            // Needs a graphical askpass program; sudo -A fails cleanly when none is configured.
            let child = Command::new(p)
                .arg("-A")
                .arg(&exe)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .context("cannot start sudo")?;
            return Ok(child);
        }
        bail!("neither pkexec nor sudo is available; run this program as root to write to a card");
    }
    #[allow(unreachable_code)]
    {
        bail!("privilege elevation is not supported on this platform; run the program as an administrator")
    }
}

#[cfg(target_os = "linux")]
fn which(name: &str) -> Result<PathBuf> {
    for dir in std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default() {
        let c = dir.join(name);
        if c.is_file() {
            return Ok(c);
        }
    }
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let c = Path::new(dir).join(name);
        if c.is_file() {
            return Ok(c);
        }
    }
    bail!("{name} not found")
}

/// Path of the cancel marker for a progress file.
pub fn cancel_path(progress_file: &Path) -> PathBuf {
    let mut p = progress_file.as_os_str().to_owned();
    p.push(".cancel");
    PathBuf::from(p)
}
