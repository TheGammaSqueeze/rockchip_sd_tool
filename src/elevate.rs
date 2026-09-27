//! Running the write with the privileges raw disk access needs.
//!
//! Windows: the executable carries a `requireAdministrator` manifest, so the whole program already
//! runs elevated. Linux and macOS: the GUI launches a second copy of itself as root through
//! `pkexec` (or `sudo`) and `osascript ... with administrator privileges`; that helper writes JSON
//! progress lines to a file the GUI polls, and stops when a `.cancel` file appears next to it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};

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

#[allow(dead_code)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
    #[cfg(target_os = "macos")]
    {
        let mut cmd = shell_quote(&exe.to_string_lossy());
        for a in &args {
            cmd.push(' ');
            cmd.push_str(&shell_quote(a));
        }
        let script = format!("do shell script {} with administrator privileges", applescript_quote(&cmd));
        let child = Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start osascript")?;
        return Ok(child);
    }
    #[allow(unreachable_code)]
    {
        bail!("privilege elevation is not supported on this platform; run the program as an administrator")
    }
}

#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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
