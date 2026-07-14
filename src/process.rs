/// Process management — spawn, wait, kill background jobs.
///
/// Replaces the original Python version's `subprocess.Popen` +
/// `start_new_session=True` pattern.
///
/// For `cmd_run`, we fork the process: the child handles allocation and
/// execution while the parent returns immediately — maximizing throughput
/// for OpenFOAM multi-case batch submission.
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Fork the current process and detach the child (setsid).
/// Returns `true` in the child, `false` in the parent.
/// The parent returns immediately; the child continues.
///
/// # Safety
/// `fork()` is called while the process is single-threaded (before any threads
/// are spawned), which makes it safe in practice. This constraint must be
/// maintained by all callers.
pub fn fork_detach() -> io::Result<bool> {
    // SAFETY: called single-threaded at program start, before any threads exist.
    let pid =
        unsafe { nix::unistd::fork() }.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    if pid.is_parent() {
        // Parent: done
        return Ok(false);
    }

    // Child: create new session to detach from terminal
    let _ = nix::unistd::setsid();
    Ok(true)
}

/// Redirect stderr of the current process to the given file path.
/// All subsequent eprintln! output goes to the file instead of the terminal.
pub fn redirect_stderr(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let fd = file.as_raw_fd();
    nix::unistd::dup2(fd, 2).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    // Don't close the file — it will be closed on exit
    std::mem::forget(file);
    Ok(())
}

/// Spawn a command detached in its own process group.
///
/// stdout/stderr are redirected to `output_path`.
/// The child's stdin is closed (DEVNULL).
/// CORESCHED_JOB_ID and CORESCHED_PIN are set in the environment.
pub fn spawn_detached(
    program: &str,
    args: &[String],
    output_path: &Path,
    envs: &[(String, String)],
) -> io::Result<Child> {
    let out_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(output_path)?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(out_file.try_clone()?)
        .stderr(out_file.try_clone()?)
        .envs(envs.iter().cloned())
        .process_group(0);

    let child = cmd.spawn()?;
    Ok(child)
}

/// Wait for a child process to exit and return its exit code.
pub fn wait_for_child(child: &mut Child) -> io::Result<i32> {
    let status = child.wait()?;
    Ok(status.code().unwrap_or(-1))
}

/// Kill a process by PID, with SIGTERM + graceful fallback to SIGKILL.
pub fn cancel_job(pid: u32) -> io::Result<()> {
    let pid = nix::unistd::Pid::from_raw(pid as i32);

    // SIGTERM first
    if let Err(e) = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM) {
        if e != nix::errno::Errno::ESRCH {
            return Err(io::Error::new(io::ErrorKind::Other, e));
        }
        return Ok(()); // already dead
    }

    // Small grace period (same as Python: sleep 0.2)
    std::thread::sleep(Duration::from_millis(200));

    // SIGKILL fallback
    if let Err(e) = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL) {
        if e != nix::errno::Errno::ESRCH {
            return Err(io::Error::new(io::ErrorKind::Other, e));
        }
    }

    Ok(())
}

/// Check if a process is still alive.
pub fn is_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_is_alive_self() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn test_is_alive_nonexistent() {
        assert!(!is_alive(999_999_999));
    }

    #[test]
    fn test_spawn_detached_echo() {
        let dir = std::env::temp_dir().join("coresched-test");
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("test-echo.out");

        let mut child = spawn_detached("echo", &["hello".to_string()], &out, &[]).unwrap();
        let code = wait_for_child(&mut child).unwrap();
        assert_eq!(code, 0);

        let content = fs::read_to_string(&out).unwrap();
        assert_eq!(content.trim(), "hello");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_spawn_detached_inherits_parent_env() {
        let dir = std::env::temp_dir().join("coresched-test-env");
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("test-env.out");

        let home = std::env::var("HOME").unwrap();
        let mut child = spawn_detached(
            "bash",
            &["-c".to_string(), "printf '%s' \"$HOME\"".to_string()],
            &out,
            &[("CORESCHED_JOB_ID".to_string(), "test".to_string())],
        )
        .unwrap();
        let code = wait_for_child(&mut child).unwrap();
        assert_eq!(code, 0);

        let content = fs::read_to_string(&out).unwrap();
        assert_eq!(content, home);

        fs::remove_dir_all(&dir).ok();
    }
}
