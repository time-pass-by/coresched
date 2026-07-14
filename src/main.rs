/// coresched — CPU core scheduler for Legend
///
/// Rust rewrite of the original Bash + Python3 implementation.
/// Usage:
///   coresched run --cfd N [--] <cmd...>    CFD job (physical cores)
///   coresched run --cpus N [--] <cmd...>   Python job (logical CPUs)
///   coresched list                          List queue
///   coresched status                        Show CPU allocation map
///   coresched cancel <jid>                  Cancel a job
///   coresched wait <jid>                    Wait for a job to finish
///   coresched tail <jid>                    Show job output
mod cli;
mod pin;
mod process;
mod progress;
mod scheduler;
mod state;
mod topology;

use cli::Cli;
use progress::{calc_progress, print_row, print_table_header};
use scheduler::{orphan_cleanup, JobDispatchInfo};
use state::{
    init, job_output_path, job_progress_path, load_from_disk, next_jid, save_to_disk, with_lock,
    JobEntry, JobPriority,
};
use std::time::{Duration, SystemTime};

/// Format elapsed time in seconds.
fn elapsed_since(t: Option<f64>) -> String {
    match t {
        Some(start) => {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            format!("{:>7.0}s", now - start)
        }
        None => "       -".into(),
    }
}

/// Validate the requested size against the current reservation policy, not the
/// momentary free capacity. This avoids admitting a priority 0 or 1 job that
/// can never be scheduled while a physical core is reserved for priority 2.
fn validate_request(job_type: &str, n: u8, priority: JobPriority) -> Result<(), String> {
    let result = with_lock(|| -> Result<(), String> {
        init().map_err(|error| error.to_string())?;
        let state = load_from_disk().map_err(|error| error.to_string())?;
        let maximum = match job_type {
            "cfd" => scheduler::eligible_physical_cores(&state, priority).len(),
            "py" => scheduler::eligible_cpus(&state, priority).len(),
            _ => 0,
        };

        if n as usize <= maximum {
            return Ok(());
        }

        let unit = if job_type == "cfd" {
            "物理核"
        } else {
            "逻辑CPU"
        };
        let reserved = scheduler::reserved_physical_cores(&state);
        let reservation = if reserved.is_empty() {
            String::new()
        } else {
            format!(
                "；Core {} 已保留给优先级 2 任务",
                reserved
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Err(format!(
            "coresched: {} 优先级最多可申请 {} 个{}{}",
            priority, maximum, unit, reservation
        ))
    })
    .map_err(|error| error.to_string())?;

    result
}

fn concise_path(value: &str) -> String {
    let value = value.trim_matches(|character| character == '\'' || character == '"');
    let parts: Vec<&str> = value
        .trim_end_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let summary = if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        value.to_string()
    };

    if summary.chars().count() <= 72 {
        summary
    } else {
        format!("{}...", summary.chars().take(69).collect::<String>())
    }
}

/// Render one compact command or script path for queue tables.
fn command_summary(command: Option<&str>) -> String {
    let command = match command.map(str::trim).filter(|value| !value.is_empty()) {
        Some(command) => command,
        None => return "-".to_string(),
    };
    let arguments: Vec<String> = serde_json::from_str(command).unwrap_or_else(|_| {
        command
            .split_whitespace()
            .map(ToString::to_string)
            .collect()
    });
    let is_script = |argument: &String| {
        let name = argument.rsplit('/').next().unwrap_or(argument);
        [".py", ".sh", ".bash", ".jl", ".R"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
    };
    let is_path = |argument: &String| {
        argument.contains('/') && !argument.starts_with('-') && !argument.contains('=')
    };
    let is_command = |argument: &String| {
        !argument.starts_with('-')
            && !argument.contains('=')
            && argument != "env"
            && argument != "command"
    };

    arguments
        .iter()
        .find(|argument| is_script(argument))
        .or_else(|| arguments.iter().find(|argument| is_path(argument)))
        .or_else(|| arguments.iter().find(|argument| is_command(argument)))
        .map(|argument| concise_path(argument))
        .unwrap_or_else(|| concise_path(command))
}

/// Run command: register as pending, fork, parent returns immediately.
/// The child handles allocation, pinning, execution, and cleanup.
fn cmd_run(
    cfd: Option<u8>,
    cpus: Option<u8>,
    priority: JobPriority,
    timeout: u64,
    command: Vec<String>,
) -> i32 {
    let n = match (cfd, cpus) {
        (Some(n), _) => n,
        (_, Some(n)) => n,
        (None, None) => {
            eprintln!("coresched: 必须指定 --cfd 或 --cpus");
            return 1;
        }
    };
    let job_type = if cfd.is_some() { "cfd" } else { "py" };
    let label = if job_type == "cfd" { "CFD" } else { "PY" };
    let unit = if job_type == "cfd" {
        "物理核"
    } else {
        "逻辑CPU"
    };
    if let Err(error) = validate_request(job_type, n, priority) {
        eprintln!("{}", error);
        return 1;
    }

    // ── Phase 1: Register as pending (with lock) ──
    let (jid, output_path) = with_lock(|| {
        init().ok();
        let mut s = load_from_disk().unwrap_or_default();
        orphan_cleanup(&mut s);
        let jid = next_jid(&mut s);
        let out_path = job_output_path(&jid);
        std::fs::create_dir_all(out_path.parent().unwrap()).ok();
        s.pending.insert(
            jid.clone(),
            state::JobEntry {
                jid: jid.clone(),
                job_type: job_type.to_string(),
                need: n as u64,
                pid: Some(std::process::id()),
                submit: Some(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs_f64(),
                ),
                start: None,
                cmd: Some(command.join(" ")),
                output: Some(out_path.to_string_lossy().to_string()),
                cores: None,
                cpus: None,
                priority,
            },
        );
        save_to_disk(&s).ok();
        (jid, out_path)
    })
    .expect("failed to register job");

    // ── Phase 2: Fork — parent returns; child does the work ──
    match process::fork_detach() {
        Ok(true) => {
            // ── CHILD: allocation, pin, exec, cleanup ──
            cmd_run_child(&jid, job_type, n, priority, timeout, &command, &output_path);
            // child never reaches here; exec or exit inside cmd_run_child
            std::process::exit(0);
        }
        Ok(false) => {
            // ── PARENT: confirm and return immediately ──
            println!("✓ 已提交 [{}:{}#{}] {} {}", label, priority, jid, n, unit);
            println!("  输出: {}", output_path.display());
            println!("  coresched list | status  → 查看");
            println!("  coresched wait {}      → 等待完成", jid);
            0
        }
        Err(e) => {
            eprintln!("coresched: fork 失败: {}", e);
            1
        }
    }
}

/// Child-side run: allocate resources, pin CPU, spawn command, wait, cleanup.
/// Called after fork+setsid. Exits with child's exit code.
fn cmd_run_child(
    jid: &str,
    job_type: &str,
    n: u8,
    priority: JobPriority,
    timeout: u64,
    command: &[String],
    output_path: &std::path::Path,
) {
    // Redirect stderr to output file so all progress messages go to the log
    process::redirect_stderr(output_path).ok();

    // ── Update pending PID to child's PID ──
    // Phase 1 registered this job with the PARENT's PID. After fork,
    // the parent exits immediately; if we don't update the PID,
    // orphan_cleanup will see the dead parent PID and remove
    // this entry from pending, making coresched list show nothing.
    with_lock(|| {
        let mut s = load_from_disk().unwrap_or_default();
        if let Some(entry) = s.pending.get_mut(jid) {
            entry.pid = Some(std::process::id());
            save_to_disk(&s).ok();
        }
    })
    .ok();

    // ── Wait for & allocate resources ──
    let alloc_start = SystemTime::now();
    let pin_cpus: Vec<u8> = loop {
        let maybe = with_lock(|| -> Option<Vec<u8>> {
            init().ok()?;
            let mut s = load_from_disk().ok()?;
            orphan_cleanup(&mut s);

            if scheduler::has_higher_priority_waiting(&s, priority, Some(jid)) {
                save_to_disk(&s).ok()?;
                return None;
            }

            let need = scheduler::needed_for_priority(&s, job_type, n, priority);
            if need > 0 {
                save_to_disk(&s).ok()?;
                return None;
            }

            let cores = if job_type == "cfd" {
                scheduler::alloc_cfd_for_priority(&mut s, n, priority)
            } else {
                scheduler::alloc_py_for_priority(&mut s, n, priority)
            };
            s.pending.remove(jid);

            let cpus: Vec<u8> = if job_type == "cfd" {
                cores
                    .iter()
                    .flat_map(|&core| {
                        let (first, second) = topology::logical_cpus(core).unwrap();
                        [first, second]
                    })
                    .collect()
            } else {
                cores.iter().take(n as usize).copied().collect()
            };

            s.pids.insert(
                jid.to_string(),
                state::JobEntry {
                    jid: jid.to_string(),
                    job_type: job_type.to_string(),
                    need: n as u64,
                    pid: None,
                    submit: None,
                    start: Some(
                        SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_secs_f64(),
                    ),
                    cmd: Some(command.join(" ")),
                    output: Some(output_path.to_string_lossy().to_string()),
                    cores: if job_type == "cfd" { Some(cores) } else { None },
                    cpus: if job_type == "py" {
                        Some(cpus.clone())
                    } else {
                        None
                    },
                    priority,
                },
            );
            save_to_disk(&s).ok()?;
            Some(cpus)
        });

        match maybe {
            Ok(Some(cpus)) => break cpus,
            _ => {
                // Check allocation timeout
                if timeout > 0 {
                    let elapsed = SystemTime::now()
                        .duration_since(alloc_start)
                        .unwrap_or_default();
                    if elapsed.as_secs() > timeout {
                        eprintln!("coresched: [{}] 分配超时，已等待 {}s，放弃", jid, timeout);
                        with_lock(|| {
                            let mut s = load_from_disk().unwrap_or_default();
                            s.pending.remove(jid);
                            save_to_disk(&s).ok();
                        })
                        .ok();
                        std::process::exit(1);
                    }
                }
                eprintln!(
                    "coresched: 等待资源释放... (need {} {})",
                    n,
                    if job_type == "cfd" {
                        "物理核"
                    } else {
                        "逻辑CPU"
                    }
                );
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let pin_str = pin::pin_str(&pin_cpus);
    eprintln!("coresched: [{}] 已分配 {}", jid, pin_str);

    // ── Bind CPU affinity ──
    pin::pin_to_cpus(&pin_cpus).expect("failed to bind CPU affinity");

    // ── Build environment for child ──
    let env_vars = vec![
        ("CORESCHED_JOB_ID".to_string(), jid.to_string()),
        ("CORESCHED_PIN".to_string(), pin_str.clone()),
        ("CORESCHED_CPU_COUNT".to_string(), n.to_string()),
        (
            "CORESCHED_PROGRESS_FILE".to_string(),
            job_progress_path(jid).to_string_lossy().to_string(),
        ),
    ];

    // ── Spawn command, redirect to output file ──
    let program = command[0].clone();
    let args: Vec<String> = command[1..].to_vec();

    let mut child = process::spawn_detached(&program, &args, output_path, &env_vars)
        .expect("failed to spawn command");

    // Update PID in state
    with_lock(|| {
        let mut s = load_from_disk().unwrap_or_default();
        if let Some(entry) = s.pids.get_mut(jid) {
            entry.pid = Some(child.id());
        }
        save_to_disk(&s).expect("failed to save state");
    })
    .ok();

    // ── Wait for completion ──
    let exit_code = process::wait_for_child(&mut child).unwrap_or(-1);

    // ── Free resources ──
    with_lock(|| {
        let mut s = load_from_disk().unwrap_or_default();
        scheduler::free_job(&mut s, jid);
        save_to_disk(&s).expect("failed to save state");
    })
    .ok();

    eprintln!("coresched: [{}] 已完成，释放 {}", jid, pin_str);

    dispatch_queued_jobs();

    std::process::exit(exit_code);
}

/// ── Enqueue: write job to persistent queue (no fork, no waiter) ──
fn cmd_enqueue(
    cfd: Option<u8>,
    cpus: Option<u8>,
    priority: JobPriority,
    command: Vec<String>,
) -> i32 {
    let n = match (cfd, cpus) {
        (Some(n), _) => n,
        (_, Some(n)) => n,
        (None, None) => {
            eprintln!("coresched: 必须指定 --cfd 或 --cpus");
            return 1;
        }
    };
    let job_type = if cfd.is_some() { "cfd" } else { "py" };
    let label = if job_type == "cfd" { "CFD" } else { "PY" };
    let unit = if job_type == "cfd" {
        "物理核"
    } else {
        "逻辑CPU"
    };
    if let Err(error) = validate_request(job_type, n, priority) {
        eprintln!("{}", error);
        return 1;
    }

    let jid = with_lock(|| {
        init().ok();
        let mut s = load_from_disk().unwrap_or_default();
        orphan_cleanup(&mut s);
        let jid = next_jid(&mut s);
        let out_path = job_output_path(&jid);
        std::fs::create_dir_all(out_path.parent().unwrap()).ok();

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        s.queued.push_back(JobEntry {
            jid: jid.clone(),
            job_type: job_type.to_string(),
            need: n as u64,
            pid: None,
            submit: Some(now),
            start: None,
            cmd: Some(serde_json::to_string(&command).unwrap_or_default()),
            output: Some(out_path.to_string_lossy().to_string()),
            cores: None,
            cpus: None,
            priority,
        });
        save_to_disk(&s).ok();
        jid
    })
    .expect("enqueue failed");

    println!("✓ 已入队 [{}:{}#{}] {} {}", label, priority, jid, n, unit);
    println!("  coresched status → 查看队列");

    dispatch_queued_jobs();

    0
}

/// Configure the priority-2-only physical cores. Existing jobs keep their
/// affinity; the policy applies to every allocation made afterwards.
fn cmd_reserve_core(cores: Vec<u8>, clear: bool) -> i32 {
    if clear {
        let previous = with_lock(|| -> Result<Vec<u8>, String> {
            init().map_err(|error| error.to_string())?;
            let mut state = load_from_disk().map_err(|error| error.to_string())?;
            let previous = state.reserved_cores();
            state.set_reserved_cores(Vec::new());
            save_to_disk(&state).map_err(|error| error.to_string())?;
            Ok(previous)
        });

        return match previous {
            Ok(Ok(previous)) if !previous.is_empty() => {
                println!(
                    "已取消 Core {} 的优先级 2 保留",
                    previous
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                dispatch_queued_jobs();
                0
            }
            Ok(Ok(_)) => {
                println!("当前没有优先级 2 保留核");
                dispatch_queued_jobs();
                0
            }
            Ok(Err(error)) => {
                eprintln!("coresched: 无法更新保留策略: {}", error);
                1
            }
            Err(error) => {
                eprintln!("coresched: 无法更新保留策略: {}", error);
                1
            }
        };
    }

    if cores.is_empty()
        || cores
            .iter()
            .any(|&core| topology::logical_cpus(core).is_none())
    {
        eprintln!(
            "coresched: 物理核必须在 0-{} 范围内",
            topology::num_physical_cores().saturating_sub(1)
        );
        return 1;
    }
    let mut requested = Vec::new();
    for core in cores {
        if !requested.contains(&core) {
            requested.push(core);
        }
    }

    let previous = with_lock(|| -> Result<Vec<u8>, String> {
        init().map_err(|error| error.to_string())?;
        let mut state = load_from_disk().map_err(|error| error.to_string())?;
        let previous = state.reserved_cores();
        state.set_reserved_cores(requested.clone());
        save_to_disk(&state).map_err(|error| error.to_string())?;
        Ok(previous)
    });

    match previous {
        Ok(Ok(previous)) => {
            let description = requested
                .iter()
                .map(|&core| {
                    let (first, second) = topology::logical_cpus(core).unwrap();
                    format!("Core {} ({},{})", core, first, second)
                })
                .collect::<Vec<_>>()
                .join("；");
            if previous == requested {
                println!("{} 已保持为优先级 2 专用", description);
            } else {
                println!("{} 已保留，仅优先级 2 任务可新分配使用", description);
                println!("现有运行作业的绑定不变；该策略会在后续分配时生效。");
            }
            dispatch_queued_jobs();
            0
        }
        Ok(Err(error)) => {
            eprintln!("coresched: 无法更新保留策略: {}", error);
            1
        }
        Err(error) => {
            eprintln!("coresched: 无法更新保留策略: {}", error);
            1
        }
    }
}

/// ── Dispatch queued jobs: try to allocate and spawn as many as possible ──
fn dispatch_queued_jobs() {
    loop {
        let info = with_lock(|| {
            let mut s = load_from_disk().unwrap_or_default();
            orphan_cleanup(&mut s);
            scheduler::try_dispatch_one(&mut s)
        })
        .ok()
        .flatten();

        let info = match info {
            Some(i) => i,
            None => break,
        };

        match process::fork_detach() {
            Ok(true) => {
                dispatch_child_exec(&info);
                std::process::exit(0);
            }
            Ok(false) => {
                continue;
            }
            Err(_) => {
                let jid = info.jid.clone();
                let jt = info.job_type.clone();
                let priority = info.priority;
                let n = info.need;
                let cmd = info.cmd.clone();
                let output = info.output.clone();
                let submit = info.submit;
                with_lock(|| {
                    let mut s = load_from_disk().unwrap_or_default();
                    scheduler::free_job(&mut s, &jid);
                    let entry = JobEntry {
                        jid,
                        job_type: jt,
                        need: n as u64,
                        pid: None,
                        submit,
                        start: None,
                        cmd: Some(cmd),
                        output: Some(output),
                        cores: None,
                        cpus: None,
                        priority,
                    };
                    s.queued.push_front(entry);
                    save_to_disk(&s).ok();
                })
                .ok();
                break;
            }
        }
    }
}

/// ── Child side of dispatch: pin, spawn command, wait, free, dispatch next ──
fn dispatch_child_exec(info: &JobDispatchInfo) {
    let output_path = std::path::Path::new(&info.output);
    process::redirect_stderr(output_path).ok();

    let pin_str = pin::pin_str(&info.pin_cpus);
    eprintln!("coresched: [{}] 已分配 {}", info.jid, pin_str);

    pin::pin_to_cpus(&info.pin_cpus).ok();

    if info.args.is_empty() {
        eprintln!("coresched: [{}] 空命令，放弃", info.jid);
        with_lock(|| {
            let mut s = load_from_disk().unwrap_or_default();
            scheduler::free_job(&mut s, &info.jid);
            save_to_disk(&s).ok();
        })
        .ok();
        dispatch_queued_jobs();
        std::process::exit(1);
    }

    let program = &info.args[0];
    let args = &info.args[1..];

    let env_vars = vec![
        ("CORESCHED_JOB_ID".to_string(), info.jid.clone()),
        ("CORESCHED_PIN".to_string(), pin_str.clone()),
        ("CORESCHED_CPU_COUNT".to_string(), info.need.to_string()),
        (
            "CORESCHED_PROGRESS_FILE".to_string(),
            job_progress_path(&info.jid).to_string_lossy().to_string(),
        ),
    ];

    let mut child = process::spawn_detached(program, args, output_path, &env_vars)
        .expect("dispatch spawn failed");

    with_lock(|| {
        let mut s = load_from_disk().unwrap_or_default();
        if let Some(e) = s.pids.get_mut(&info.jid) {
            e.pid = Some(child.id());
        }
        save_to_disk(&s).ok();
    })
    .ok();

    let exit_code = process::wait_for_child(&mut child).unwrap_or(-1);

    with_lock(|| {
        let mut s = load_from_disk().unwrap_or_default();
        scheduler::free_job(&mut s, &info.jid);
        save_to_disk(&s).ok();
    })
    .ok();

    eprintln!("coresched: [{}] 已完成，释放 {}", info.jid, pin_str);

    dispatch_queued_jobs();

    std::process::exit(exit_code);
}

/// List queued and running jobs.
fn cmd_list() {
    with_lock(|| {
        init().ok();
        // Read-only commands must not mutate scheduler state. In particular,
        // a status check can run from a different PID namespace than the
        // dispatcher and cannot reliably probe host job PIDs.
        let s = load_from_disk().unwrap_or_default();

        let running_count = s.pids.len();
        let pending_count = s.pending.len();
        let queued_count = s.queued.len();
        let total_queued = pending_count + queued_count;

        let cfd_cores: u64 = s.pending.values().filter(|e| e.job_type == "cfd").map(|e| e.need).sum();
        let cfd_cores_q: u64 = s.queued.iter().filter(|e| e.job_type == "cfd").map(|e| e.need).sum();
        let py_cpus: u64 = s.pending.values().filter(|e| e.job_type == "py").map(|e| e.need).sum();
        let py_cpus_q: u64 = s.queued.iter().filter(|e| e.job_type == "py").map(|e| e.need).sum();
        let priority_two_waiting = s
            .pending
            .values()
            .filter(|e| e.priority == JobPriority::P2)
            .count()
            + s
                .queued
                .iter()
                .filter(|e| e.priority == JobPriority::P2)
                .count();

        println!(
            "运行: {} | 排队: {} (pending: {} + queued: {}) | P2等待: {} | CFD: {}核 | Python: {}CPU",
            running_count, total_queued, pending_count, queued_count,
            priority_two_waiting, cfd_cores + cfd_cores_q, py_cpus + py_cpus_q,
        );

        // Running
        if !s.pids.is_empty() {
            println!("── 运行中 ──");
            let mut entries: Vec<_> = s.pids.iter().collect();
            entries.sort_by_key(|(_, e)| {
                e.cores.as_ref().and_then(|c| c.first().copied())
                    .or_else(|| e.cpus.as_ref().and_then(|c| c.first().copied()))
                    .unwrap_or(99)
            });
            for (jid, e) in &entries {
                let cs = e.cores.as_ref().or(e.cpus.as_ref());
                let cs_str = cs.map(|v| format!("{:?}", v)).unwrap_or_default();
                println!(
                    "  {:<6} {:<8} {:<8} {:<14} {:<8} {}",
                    jid,
                    e.priority,
                    e.job_type,
                    cs_str,
                    e.pid.unwrap_or(0),
                    elapsed_since(e.start),
                );
            }
        }

        // Pending (waiter processes)
        if !s.pending.is_empty() {
            println!();
            println!("── 等待分配 (pending) ──");
            println!("{:^8} {:^8} {:^8} {:^10} {:^8} {:^10} {:^0}", "作业", "优先级", "类型", "需求", "PID", "等待", "命令");
            println!("{}", "-".repeat(98));
            for (jid, e) in &s.pending {
                let unit = if e.job_type == "cfd" { "核" } else { "CPU" };
                println!(
                    "  {:<6} {:<8} {:<8} {:<10} {:<8} {}   {}",
                    jid,
                    e.priority,
                    e.job_type,
                    format!("{}{}", e.need, unit),
                    e.pid.unwrap_or(0),
                    elapsed_since(e.submit),
                    command_summary(e.cmd.as_deref()),
                );
            }
        }

        // Queued (durable queue)
        if !s.queued.is_empty() {
            println!();
            println!("── 内置队列 (queued) ──");
            println!("{:^8} {:^8} {:^8} {:^10} {:^12} {:^0}", "作业", "优先级", "类型", "需求", "等待", "命令");
            println!("{}", "-".repeat(82));
            for e in &s.queued {
                let unit = if e.job_type == "cfd" { "核" } else { "CPU" };
                println!(
                    "  {:<6} {:<8} {:<8} {:<10} {}   {}",
                    e.jid,
                    e.priority,
                    e.job_type,
                    format!("{}{}", e.need, unit),
                    elapsed_since(e.submit),
                    command_summary(e.cmd.as_deref()),
                );
            }
        }
    })
    .ok();
}

/// Show CPU allocation map.
fn cmd_status() {
    with_lock(|| {
        init().ok();
        // Keep status read-only; stale-job cleanup happens when the host-side
        // dispatcher allocates the next queued job.
        let s = load_from_disk().unwrap_or_default();

        let reserved_cores = scheduler::reserved_physical_cores(&s);

        println!("\n{:^8} {:^8} {:^12} {}", "物理核", "类型", "CPUs", "策略");
        println!("{}", "-".repeat(48));
        for c in topology::physical_cores() {
            let (a, b) = topology::logical_cpus(c).unwrap();
            let cfd_used = s.cfd.get(&c.to_string()).copied().unwrap_or(false);
            let py_used = s
                .cpus
                .get(&a.to_string())
                .copied()
                .unwrap_or(None)
                .is_some()
                || s.cpus
                    .get(&b.to_string())
                    .copied()
                    .unwrap_or(None)
                    .is_some();
            let label = if cfd_used {
                "CFD"
            } else if py_used {
                "PY"
            } else {
                "空闲"
            };
            let policy = if reserved_cores.contains(&c) {
                "仅 P2"
            } else {
                ""
            };
            println!(
                "  Core {:<4} {:<8} {:<12} {}",
                c,
                label,
                format!("{},{}", a, b),
                policy
            );
        }

        println!();
        let mut cpus_status = String::new();
        for c in topology::all_cpus() {
            let v = s.cpus.get(&c.to_string()).copied().unwrap_or(None);
            let high_only = reserved_cores.iter().any(|&core| {
                let (first, second) = topology::logical_cpus(core).unwrap();
                c == first || c == second
            });
            cpus_status.push(if v.is_some() {
                '█'
            } else if high_only {
                'H'
            } else if topology::is_schedulable_cpu(c) {
                '░'
            } else {
                '·'
            });
        }
        let cpus_chars: Vec<char> = cpus_status.chars().collect();
        println!("逻辑CPU: 0 1 2 3 4 5 6 7  8 9 10 11 12 13 14 15");
        println!(
            "         {}  {}",
            cpus_chars[..8].iter().collect::<String>(),
            cpus_chars[8..].iter().collect::<String>(),
        );
        let priority_one_capacity = scheduler::eligible_cpus(&s, JobPriority::P1).len();
        let priority_two_cpus = scheduler::eligible_cpus(&s, JobPriority::P2);
        let priority_two_capacity = priority_two_cpus.len();
        if !reserved_cores.is_empty() {
            let reserved_description = reserved_cores
                .iter()
                .map(|&core| {
                    let (first, second) = topology::logical_cpus(core).unwrap();
                    format!("Core {} ({},{})", core, first, second)
                })
                .collect::<Vec<_>>()
                .join("；");
            println!(
                "         P0/P1 上限: {} 逻辑CPU；P2 上限: {}；P2 优先: {}；H = {} 的 P2 专用逻辑CPU",
                priority_one_capacity,
                priority_two_capacity,
                priority_two_cpus
                    .iter()
                    .take(reserved_cores.len() * 2)
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                reserved_description
            );
        } else {
            println!(
                "         --cpus 可调度上限: {} 逻辑CPU；· 表示保留给物理核/系统",
                priority_one_capacity
            );
        }
        println!();

        // Running jobs
        if s.pids.is_empty() {
            println!("  (无运行中的作业)");
        } else {
            println!(
                "{:^6} {:^8} {:^8} {:^14} {:^8} {:^10} {:^8} {:^18} {}",
                "作业", "优先级", "类型", "CPUs", "PID", "耗时", "进度", "ETA", "阶段"
            );
            println!("{}", "-".repeat(120));
            // Sort by first allocated core/CPU for stable output
            let mut entries: Vec<_> = s.pids.iter().collect();
            entries.sort_by_key(|(_, e)| {
                e.cores
                    .as_ref()
                    .and_then(|c| c.first().copied())
                    .or_else(|| e.cpus.as_ref().and_then(|c| c.first().copied()))
                    .unwrap_or(99)
            });
            for (jid, e) in &entries {
                let cs = e.cores.as_ref().or(e.cpus.as_ref());
                let cs_str = cs.map(|v| format!("{:?}", v)).unwrap_or_default();
                let progress = calc_progress(e);
                let pct = progress
                    .progress_pct
                    .map(|value| format!("{value:.1}%"))
                    .unwrap_or_else(|| "-".to_string());
                let eta = progress
                    .eta_seconds
                    .map(progress::format_duration)
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "  {:<4} {:<8} {:<8} {:<14} {:<8} {:<10} {:<8} {:<18} {}",
                    jid,
                    e.priority,
                    e.job_type,
                    cs_str,
                    e.pid.unwrap_or(0),
                    elapsed_since(e.start),
                    pct,
                    eta,
                    progress.phase.unwrap_or_else(|| "-".to_string()),
                );
            }
        }

        // Pending
        if !s.pending.is_empty() {
            println!();
            println!(
                "{:^8} {:^8} {:^8} {:^10} {:^8} {:^10} {:^0}",
                "等待作业", "优先级", "类型", "需求", "PID", "等待", "命令"
            );
            println!("{}", "-".repeat(98));
            for (jid, e) in &s.pending {
                let unit = if e.job_type == "cfd" { "核" } else { "CPU" };
                println!(
                    "  {:<6} {:<8} {:<8} {:<10} {:<8} {}   {}",
                    jid,
                    e.priority,
                    e.job_type,
                    format!("{}{}", e.need, unit),
                    e.pid.unwrap_or(0),
                    elapsed_since(e.submit),
                    command_summary(e.cmd.as_deref()),
                );
            }
        }

        // Queued
        if !s.queued.is_empty() {
            println!();
            println!("── 内置队列 (queued: {}) ──", s.queued.len());
            println!(
                "{:^8} {:^8} {:^8} {:^10} {:^12} {:^0}",
                "作业", "优先级", "类型", "需求", "等待", "命令"
            );
            println!("{}", "-".repeat(82));
            for e in &s.queued {
                let unit = if e.job_type == "cfd" { "核" } else { "CPU" };
                println!(
                    "  {:<6} {:<8} {:<8} {:<10} {}   {}",
                    e.jid,
                    e.priority,
                    e.job_type,
                    format!("{}{}", e.need, unit),
                    elapsed_since(e.submit),
                    command_summary(e.cmd.as_deref()),
                );
            }
        }
    })
    .ok();
}

/// Cancel a job by JID.
fn cmd_cancel(jid: &str) {
    // Check queued first (no PID, just remove)
    let removed = with_lock(|| -> bool {
        let mut s = load_from_disk().unwrap_or_default();
        if let Some(pos) = s.queued.iter().position(|e| e.jid == jid) {
            s.queued.remove(pos);
            save_to_disk(&s).ok();
            return true;
        }
        false
    })
    .ok()
    .unwrap_or(false);

    if removed {
        println!("已从队列移除 {}", jid);
        return;
    }

    let pid = with_lock(|| -> Option<u32> {
        let s = load_from_disk().ok()?;
        let entry = s.pids.get(jid).or_else(|| s.pending.get(jid));
        entry.and_then(|e| e.pid)
    });

    match pid {
        Ok(Some(pid)) if pid > 0 => {
            process::cancel_job(pid).expect("failed to cancel job");
            println!("已取消 {} (pid={})", jid, pid);
        }
        _ => {
            // Job in pids/pending but has no valid PID (orphaned).
            // Free its resources and remove it.
            let freed = with_lock(|| -> bool {
                let mut s = load_from_disk().unwrap_or_default();
                if s.pids.contains_key(jid) || s.pending.contains_key(jid) {
                    scheduler::free_job(&mut s, jid);
                    s.pending.remove(jid);
                    save_to_disk(&s).ok();
                    return true;
                }
                false
            })
            .ok()
            .unwrap_or(false);

            if freed {
                println!("已清除孤儿作业 {}", jid);
            } else {
                eprintln!("未找到 {}", jid);
            }
        }
    }
}

/// Wait for a job to finish.
fn cmd_wait(jid: &str) {
    loop {
        let alive: bool = with_lock(|| -> Option<bool> {
            let s = load_from_disk().ok()?;
            let pid = s
                .pids
                .get(jid)
                .and_then(|e| e.pid)
                .or_else(|| s.pending.get(jid).and_then(|e| e.pid))?;
            Some(process::is_alive(pid))
        })
        .ok()
        .flatten()
        .unwrap_or(false);

        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("{} 已结束", jid);
}

/// Show job output.
fn cmd_tail(jid: &str) {
    let out_path = state::job_output_path(jid);
    if !out_path.exists() {
        eprintln!("未找到 {} 输出", jid);
        return;
    }
    let content = std::fs::read_to_string(&out_path).unwrap_or_default();
    // Show last 40 lines (matching Python tail -n 40)
    let lines: Vec<&str> = content.lines().collect();
    let tail = if lines.len() > 40 {
        &lines[lines.len() - 40..]
    } else {
        &lines[..]
    };
    for line in tail {
        println!("{}", line);
    }
    if !content.ends_with('\n') {
        println!();
    }
}

/// Show job simulation progress and estimated remaining time.
fn cmd_progress(jid: Option<&str>) {
    let state = load_from_disk().unwrap_or_default();

    // If a specific JID is requested
    if let Some(jid) = jid {
        let entry = state.pids.get(jid).or_else(|| state.pending.get(jid));
        match entry {
            Some(e) => {
                let info = calc_progress(e);
                progress::print_detail(&info);
            }
            None => eprintln!("未找到作业 {}", jid),
        }
        return;
    }

    // Show all running jobs. CFD jobs retain log-derived progress; scripts can
    // opt in through CORESCHED_PROGRESS_FILE.
    let mut running: Vec<_> = state.pids.values().collect();
    if running.is_empty() {
        println!("(无运行中的作业)");
        return;
    }

    running.sort_by_key(|e| {
        e.cores
            .as_ref()
            .and_then(|c| c.first().copied())
            .or_else(|| e.cpus.as_ref().and_then(|c| c.first().copied()))
            .unwrap_or(99)
    });

    print_table_header();
    for entry in running {
        let info = calc_progress(entry);
        print_row(&info);
    }
}

fn main() {
    let cli = Cli::parse_args();

    let exit_code = match cli {
        Cli::Run {
            cfd,
            cpus,
            priority,
            timeout,
            command,
        } => cmd_run(cfd, cpus, priority, timeout, command),
        Cli::List => {
            cmd_list();
            0
        }
        Cli::Status => {
            cmd_status();
            0
        }
        Cli::Cancel { jid } => {
            cmd_cancel(&jid);
            0
        }
        Cli::Wait { jid } => {
            cmd_wait(&jid);
            0
        }
        Cli::Tail { jid } => {
            cmd_tail(&jid);
            0
        }
        Cli::Enqueue {
            cfd,
            cpus,
            priority,
            command,
        } => cmd_enqueue(cfd, cpus, priority, command),
        Cli::ReserveCore { cores, clear } => cmd_reserve_core(cores, clear),
        Cli::Progress { jid } => {
            cmd_progress(jid.as_deref());
            0
        }
    };

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::command_summary;

    #[test]
    fn command_summary_prefers_a_compact_script_path() {
        let command = r#"["env","OMP_NUM_THREADS=1","/home/tim/code/paper/timeSim/mpc/.venv-scip/bin/python","/home/tim/code/paper/timeSim/mpc/scripts/run_mr17_production_sizing.py","--grid-shard-index","7"]"#;
        assert_eq!(
            command_summary(Some(command)),
            "scripts/run_mr17_production_sizing.py"
        );
    }

    #[test]
    fn command_summary_uses_a_single_program_when_no_script_is_present() {
        assert_eq!(command_summary(Some("/bin/sh -c sleep")), "bin/sh");
    }
}
