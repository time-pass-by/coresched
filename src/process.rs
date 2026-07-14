/// Process management — spawn, wait, kill background jobs.
///
/// Replaces the original Python version's `subprocess.Popen` +
/// `start_new_session=True` pattern.
///
/// For `cmd_run`, we fork the process: the child handles allocation and
/// execution while the parent returns immediately — maximizing throughput
/// for OpenFOAM multi-case batch submission.
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
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
///
/// Scheduled commands are started as process-group leaders, but scientific
/// runners may create additional sessions for timeout isolation.  Cancelling
/// only the recorded leader would otherwise leave those descendants running
/// after the scheduler frees their CPU allocation.  Snapshot the Linux
/// process tree before signalling, then address both the inherited job group
/// and descendant-owned groups.
pub fn cancel_job(pid: u32) -> io::Result<()> {
    let processes = collect_process_tree(pid);
    let groups = job_process_groups(&processes);

    signal_groups(&groups, nix::sys::signal::Signal::SIGTERM)?;
    signal_processes(&processes, nix::sys::signal::Signal::SIGTERM)?;

    // Give cooperative solvers and wrappers a brief opportunity to flush.
    std::thread::sleep(Duration::from_millis(250));

    signal_groups(&groups, nix::sys::signal::Signal::SIGKILL)?;
    signal_processes(&processes, nix::sys::signal::Signal::SIGKILL)?;

    Ok(())
}

/// Return a snapshot consisting of the root process and every currently
/// reachable Linux child.  `/proc/.../children` is race-safe for cancellation:
/// vanished children simply produce no further work, while the process-group
/// snapshot still covers descendants that remain alive.
fn collect_process_tree(root: u32) -> Vec<u32> {
    let mut processes = vec![root];
    let mut index = 0;
    while index < processes.len() {
        let pid = processes[index];
        index += 1;
        let path = format!("/proc/{pid}/task/{pid}/children");
        let Ok(children) = fs::read_to_string(path) else {
            continue;
        };
        for child in children
            .split_whitespace()
            .filter_map(|value| value.parse::<u32>().ok())
        {
            if child > 0 && !processes.contains(&child) {
                processes.push(child);
            }
        }
    }
    processes
}

/// Every scheduled command is a group leader.  A descendant that calls
/// `setsid()` becomes the leader of an additional group, which must also be
/// signalled for cancellation to be complete.
fn job_process_groups(processes: &[u32]) -> Vec<u32> {
    let known: BTreeSet<i32> = processes.iter().map(|pid| *pid as i32).collect();
    let mut groups = BTreeSet::new();
    for pid in processes {
        let process = nix::unistd::Pid::from_raw(*pid as i32);
        if let Ok(group) = nix::unistd::getpgid(Some(process)) {
            let group = group.as_raw();
            if group > 0 && known.contains(&group) {
                groups.insert(group as u32);
            }
        }
    }
    groups.into_iter().collect()
}

fn signal_groups(groups: &[u32], signal: nix::sys::signal::Signal) -> io::Result<()> {
    for group in groups {
        // A negative PID targets the whole process group on POSIX systems.
        let target = nix::unistd::Pid::from_raw(-(*group as i32));
        signal_target(target, signal)?;
    }
    Ok(())
}

fn signal_processes(processes: &[u32], signal: nix::sys::signal::Signal) -> io::Result<()> {
    for pid in processes {
        signal_target(nix::unistd::Pid::from_raw(*pid as i32), signal)?;
    }
    Ok(())
}

fn signal_target(target: nix::unistd::Pid, signal: nix::sys::signal::Signal) -> io::Result<()> {
    match nix::sys::signal::kill(target, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(io::Error::new(io::ErrorKind::Other, error)),
    }
}

#[cfg(test)]
fn process_group_alive(group: u32) -> bool {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(-(group as i32)), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true,
    }
}

/// Check if a process is still alive.
pub fn is_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn wait_for_file_content(path: &Path) -> String {
        for _ in 0..100 {
            if let Ok(content) = fs::read_to_string(path) {
                if !content.trim().is_empty() {
                    return content;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {}", path.display());
    }

    fn wait_for_group_exit(group: u32) {
        for _ in 0..100 {
            if !process_group_alive(group) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process group {group} survived cancellation");
    }

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

    #[test]
    fn test_cancel_job_terminates_background_process_group() {
        let dir = std::env::temp_dir().join(format!(
            "coresched-test-cancel-group-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("cancel-group.out");
        let mut child = spawn_detached(
            "bash",
            &["-c".to_string(), "sleep 30 & wait".to_string()],
            &out,
            &[],
        )
        .unwrap();
        let group = child.id();

        std::thread::sleep(Duration::from_millis(50));
        cancel_job(group).unwrap();
        let _ = child.wait();
        wait_for_group_exit(group);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cancel_job_terminates_descendant_new_session() {
        let dir = std::env::temp_dir().join(format!(
            "coresched-test-cancel-session-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("cancel-session.out");
        let mut child = spawn_detached(
            "bash",
            &[
                "-c".to_string(),
                "setsid sleep 30 & child=$!; printf '%s' \"$child\"; wait".to_string(),
            ],
            &out,
            &[],
        )
        .unwrap();
        let nested_pid: u32 = wait_for_file_content(&out).trim().parse().unwrap();
        assert!(is_alive(nested_pid));

        cancel_job(child.id()).unwrap();
        let _ = child.wait();
        for _ in 0..100 {
            if !is_alive(nested_pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!is_alive(nested_pid));

        fs::remove_dir_all(&dir).ok();
    }
}
