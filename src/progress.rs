/// OpenFOAM case progress parser.
///
/// Extracts the case directory from the job command (cd "...") or from the
/// log header (`Case : /path`).  Reads `system/controlDict` for `endTime`
/// and `deltaT`, and parses `log.run` for current `Time = X`.
use crate::state::{job_progress_path, JobEntry};
use regex::Regex;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Parsed progress information for one job.
#[derive(Debug, Clone)]
pub struct ProgressInfo {
    pub jid: String,
    pub core_label: String, // e.g. "[0]" or "[0,8]"
    pub current_time: Option<f64>,
    pub start_time: Option<f64>,
    pub end_time: Option<f64>,
    pub delta_t: Option<f64>,
    pub adaptive_time_step: bool,
    pub progress_pct: Option<f64>,
    pub wall_elapsed: f64, // seconds
    pub eta_seconds: Option<f64>,
    pub steps_done: Option<u64>,
    pub steps_total: Option<u64>,
    pub case_dir: Option<PathBuf>,
    pub phase: Option<String>,
    pub error: Option<String>,
}

/// Progress contract for arbitrary scripts. Writers should write a temporary
/// sibling file and rename it into place to avoid readers seeing partial JSON.
#[derive(Debug, Deserialize)]
struct ScriptProgress {
    completed: u64,
    total: u64,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    eta_seconds: Option<f64>,
    #[serde(default)]
    updated_at: Option<f64>,
}

fn apply_script_progress(entry: &JobEntry, info: &mut ProgressInfo) -> bool {
    let path = job_progress_path(&entry.jid);
    let Ok(metadata) = fs::metadata(&path) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(script) = serde_json::from_str::<ScriptProgress>(&text) else {
        return false;
    };
    if script.total == 0 || script.completed > script.total {
        return false;
    }
    info.steps_done = Some(script.completed);
    info.steps_total = Some(script.total);
    info.phase = script.phase;
    info.progress_pct = Some((script.completed as f64 / script.total as f64 * 100.0).min(100.0));
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let updated = script
        .updated_at
        .or_else(|| {
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs_f64())
        })
        .unwrap_or(now);
    info.eta_seconds = script
        .eta_seconds
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| (value - (now - updated).max(0.0)).max(0.0));
    true
}

fn read_proc_fields(pid: u32) -> Option<Vec<String>> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end_comm = stat.rfind(')')?;
    Some(
        stat[end_comm + 1..]
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    )
}

fn process_tree(root_pid: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![root_pid];
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(fields) = read_proc_fields(pid) else {
            continue;
        };
        let Some(ppid) = fields.get(1).and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
    }

    let mut result = Vec::new();
    let mut queue = VecDeque::from([root_pid]);
    while let Some(pid) = queue.pop_front() {
        result.push(pid);
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
    result
}

/// Resolve the case that is active now, not the first case mentioned by a
/// long-lived pipeline wrapper.
fn active_case_from_process_tree(root_pid: u32) -> Option<(PathBuf, u32)> {
    let mut fallback = None;
    for pid in process_tree(root_pid) {
        let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        if !is_case_dir(&cwd) {
            continue;
        }
        let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        if cmdline.contains("openFuelCell") || cmdline.split('\0').any(|arg| arg.ends_with("Foam"))
        {
            return Some((cwd, pid));
        }
        fallback = Some((cwd, pid));
    }
    fallback
}

fn process_elapsed_seconds(pid: u32) -> Option<f64> {
    let fields = read_proc_fields(pid)?;
    let start_ticks = fields.get(19)?.parse::<f64>().ok()?;
    let uptime = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()?;
    let ticks_per_second = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    Some((uptime - start_ticks / ticks_per_second as f64).max(0.0))
}

/// Parse the case directory from a job command string.
///
/// Handles three common submission patterns:
/// 1. `cd "/path/to/case"`  (bash -c wrapper)
/// 2. `/path/to/case/run_case.sh`  (direct script invocation)
/// 3. `/path/to/runner.sh /path/to/case log.run`  (runner script with case-dir arg)
fn parse_case_dir_from_cmd(cmd: &str) -> Option<PathBuf> {
    // Try JSON array first: ["cmd","arg1","arg2",...]
    if let Ok(args) = serde_json::from_str::<Vec<String>>(cmd) {
        // Walk each arg looking for the case directory
        for arg in &args {
            // A) Existing cd / generic absolute-path patterns
            if let Some(dir) = extract_case_dir_from_text(arg) {
                return Some(dir);
            }
        }
        // Try joined string for patterns spanning multiple args
        let full = args.join(" ");
        if let Some(dir) = extract_case_dir_from_text(&full) {
            return Some(dir);
        }
    }

    // Plain string fallback (e.g. from legacy `run` command)
    if let Some(dir) = extract_case_dir_from_text(cmd) {
        return Some(dir);
    }

    None
}

fn is_case_dir(path: &Path) -> bool {
    path.is_dir() && path.join("system").join("controlDict").exists()
}

fn normalize_case_candidate(raw: &str) -> Option<PathBuf> {
    let trimmed = raw
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('\\');
    if !trimmed.starts_with('/') {
        return None;
    }

    let p = PathBuf::from(trimmed);
    if is_case_dir(&p) {
        return Some(p);
    }
    if p.is_file() {
        let parent = p.parent()?.to_path_buf();
        if is_case_dir(&parent) {
            return Some(parent);
        }
    }
    None
}

/// Try to extract a case directory from command or log text.
fn extract_case_dir_from_text(s: &str) -> Option<PathBuf> {
    // Log header: "Case   : /path/to/case"
    let re_log = Regex::new(r"(?m)^Case\s*:\s*(.+?)\s*$").ok()?;
    if let Some(cap) = re_log.captures(s) {
        if let Some(dir) = normalize_case_candidate(cap.get(1)?.as_str()) {
            return Some(dir);
        }
    }

    // cd "/path/to/case"  (JSON-escaped: cd \"/path/...\")
    let re = Regex::new(r#"cd\s+["\u{201c}]?([/][^"\s;]+)"#).ok()?;
    if let Some(cap) = re.captures(s) {
        if let Some(dir) = normalize_case_candidate(cap.get(1)?.as_str()) {
            return Some(dir);
        }
    }
    // cd /path/to/case (unquoted)
    let re2 = Regex::new(r"cd\s+(/[^\s;]+)").ok()?;
    if let Some(cap) = re2.captures(s) {
        if let Some(dir) = normalize_case_candidate(cap.get(1)?.as_str()) {
            return Some(dir);
        }
    }

    // Generic absolute-path scan:
    // - direct case path argument
    // - runner script path whose parent is the case
    let re_abs = Regex::new(r#"(?:"|')?(/[^\s"'`;]+)(?:"|')?"#).ok()?;
    for cap in re_abs.captures_iter(s) {
        if let Some(dir) = normalize_case_candidate(cap.get(1)?.as_str()) {
            return Some(dir);
        }
    }

    None
}

/// Find the solver log file in a case directory.
/// Tries known names first, then falls back to the most recently modified `log*` file.
fn find_log_file(case_dir: &std::path::Path) -> Option<PathBuf> {
    let entries = fs::read_dir(case_dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("log"))
        .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .map(|e| e.path())
}

fn read_log_content(case_dir: &Path, fallback_output: Option<&str>) -> Option<String> {
    if let Some(log_path) = find_log_file(case_dir) {
        if let Ok(content) = fs::read_to_string(log_path) {
            return Some(content);
        }
    }

    fallback_output.and_then(|p| fs::read_to_string(p).ok())
}

/// Parse endTime from an OpenFOAM controlDict content.
fn parse_control_dict_end_time(content: &str) -> Option<f64> {
    let re = Regex::new(r"endTime\s+([0-9.eE+\-]+)\s*;").ok()?;
    re.captures(content)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
}

/// Parse deltaT from an OpenFOAM controlDict content.
fn parse_control_dict_delta_t(content: &str) -> Option<f64> {
    let re = Regex::new(r"deltaT\s+([0-9.eE+\-]+)\s*;").ok()?;
    re.captures(content)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
}

fn parse_adjust_time_step(content: &str) -> bool {
    let Ok(re) = Regex::new(r"adjustTimeStep\s+(true|yes|on|1)\s*;") else {
        return false;
    };
    re.is_match(content)
}

/// Parse the last `Time = X` line from the solver log.
fn parse_last_time(log: &str) -> Option<f64> {
    let re = Regex::new(
        r"^(?:Time\s*=\s*|DYNAMIC-HOTSPOT\s+time=|EC-BLOCK-SUMMARY\s+time=)([0-9.eE+\-]+)",
    )
    .ok()?;
    log.lines()
        .rev()
        .filter_map(|line| {
            re.captures(line)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<f64>().ok())
        })
        .next()
}

fn parse_first_time(log: &str) -> Option<f64> {
    let re = Regex::new(r"^Time\s*=\s*([0-9.eE+\-]+)").ok()?;
    log.lines().find_map(|line| {
        re.captures(line)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<f64>().ok())
    })
}

fn parse_case_stage_start(case_dir: &Path, active_log: &str) -> Option<f64> {
    let mut earliest = parse_first_time(active_log);
    let entries = fs::read_dir(case_dir).ok()?;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("log") {
            continue;
        }
        let Some(value) = fs::read_to_string(entry.path())
            .ok()
            .and_then(|content| parse_first_time(&content))
        else {
            continue;
        };
        earliest = Some(earliest.map_or(value, |old| old.min(value)));
    }
    earliest
}

fn count_time_steps(log: &str, adaptive: bool) -> u64 {
    // OpenFOAM's adaptive-time include emits one `deltaT =` line per step.
    // Some solvers throttle `Time =` diagnostics, so those cannot be counted.
    let pattern = if adaptive {
        r"^deltaT\s*=\s*[0-9.eE+\-]+"
    } else {
        r"^Time\s*=\s*[0-9.eE+\-]+"
    };
    let Ok(re) = Regex::new(pattern) else {
        return 0;
    };
    log.lines().filter(|line| re.is_match(line)).count() as u64
}

/// Parse the latest numeric time directory under a case root.
fn parse_latest_time_dir(case_dir: &Path) -> Option<f64> {
    fs::read_dir(case_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let is_dir = e.file_type().ok()?.is_dir();
            if !is_dir {
                return None;
            }
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.parse::<f64>().ok()
        })
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

/// Parse the earliest numeric time directory under a case root.
fn parse_earliest_time_dir(case_dir: &Path) -> Option<f64> {
    fs::read_dir(case_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let is_dir = e.file_type().ok()?.is_dir();
            if !is_dir {
                return None;
            }
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.parse::<f64>().ok()
        })
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

/// Parse the latest simulation time from OpenFuelCell's currentSummary.dat.
///
/// This is useful for intentionally quiet runs where stdout is redirected to
/// /dev/null and no log.run exists for progress parsing.
fn parse_current_summary_time(case_dir: &Path) -> Option<f64> {
    let content = fs::read_to_string(case_dir.join("currentSummary.dat")).ok()?;
    content.lines().rev().find_map(|line| {
        let first = line.split_whitespace().next()?;
        first.parse::<f64>().ok()
    })
}

/// Format seconds into a human-readable duration string.
pub fn format_duration(secs: f64) -> String {
    if secs.is_infinite() || secs.is_nan() || secs < 0.0 {
        return "N/A".into();
    }
    let secs = secs as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m{}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// Format a Unix timestamp as HH:MM:SS.
fn format_time_of_day(ts: f64) -> String {
    let secs = ts as u64;
    let (h, m, s) = (secs / 3600 % 24, (secs % 3600) / 60, secs % 60);
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Compute progress information for a single job.
pub fn calc_progress(entry: &JobEntry) -> ProgressInfo {
    let core_label = entry
        .cores
        .as_ref()
        .map(|c| format!("{:?}", c))
        .or_else(|| entry.cpus.as_ref().map(|c| format!("{:?}", c)))
        .unwrap_or_default();

    let mut info = ProgressInfo {
        jid: entry.jid.clone(),
        core_label,
        current_time: None,
        start_time: None,
        end_time: None,
        delta_t: None,
        adaptive_time_step: false,
        progress_pct: None,
        wall_elapsed: 0.0,
        eta_seconds: None,
        steps_done: None,
        steps_total: None,
        case_dir: None,
        phase: None,
        error: None,
    };

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    info.wall_elapsed = now - entry.start.unwrap_or(now);
    if apply_script_progress(entry, &mut info) {
        return info;
    }

    // 1. Prefer the cwd of the active solver. Pipeline commands can mention
    // several cases, so command/output parsing is only a fallback.
    let cmd = entry.cmd.as_deref().unwrap_or("");
    let active = entry.pid.and_then(active_case_from_process_tree);
    let case_dir = match active
        .as_ref()
        .map(|(path, _)| path.clone())
        .or_else(|| parse_case_dir_from_cmd(cmd))
        .or_else(|| {
            entry.output.as_deref().and_then(|p| {
                fs::read_to_string(p)
                    .ok()
                    .and_then(|content| extract_case_dir_from_text(&content))
            })
        }) {
        Some(d) => d,
        None => {
            info.error = Some("命令或输出中未找到 case 路径".into());
            return info;
        }
    };
    info.case_dir = Some(case_dir.clone());

    // 2. Read solver log (try common names, fallback to most recent log*)
    let log_content = match read_log_content(&case_dir, entry.output.as_deref()) {
        Some(c) => c,
        None => {
            info.error = Some("未找到求解器日志".into());
            return info;
        }
    };

    // 3. Read controlDict
    let control_dict_path = case_dir.join("system").join("controlDict");
    if let Ok(content) = fs::read_to_string(&control_dict_path) {
        info.end_time = parse_control_dict_end_time(&content);
        info.delta_t = parse_control_dict_delta_t(&content);
        info.adaptive_time_step = parse_adjust_time_step(&content);
    }

    // 4. Parse current simulation time from log
    info.current_time = parse_last_time(&log_content)
        .or_else(|| parse_current_summary_time(&case_dir))
        .or_else(|| parse_latest_time_dir(&case_dir));
    let run_start_time = parse_first_time(&log_content);
    info.start_time = parse_case_stage_start(&case_dir, &log_content)
        .or(run_start_time)
        .or_else(|| parse_earliest_time_dir(&case_dir));

    // 5. Compute wall elapsed
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let start = entry.start.unwrap_or(now);
    info.wall_elapsed = active
        .as_ref()
        .and_then(|(_, pid)| process_elapsed_seconds(*pid))
        .unwrap_or(now - start);

    // 6. Compute progress & ETA
    if let (Some(current), Some(end)) = (info.current_time, info.end_time) {
        let begin = info.start_time.unwrap_or(0.0);
        let duration = end - begin;
        let elapsed = (current - begin).max(0.0);
        if duration > 0.0 && current <= end && elapsed >= 0.0 {
            let fraction = (elapsed / duration).clamp(0.0, 1.0);
            let pct = (fraction * 100.0).min(99.9);
            info.progress_pct = Some(pct);
            if let Some(run_begin) = run_start_time.or(info.start_time) {
                let run_simulated = current - run_begin;
                if run_simulated > 0.0 {
                    info.eta_seconds = Some(info.wall_elapsed * (end - current) / run_simulated);
                }
            }
        }

        if info.adaptive_time_step {
            let log_steps = count_time_steps(&log_content, true);
            info.steps_done = (log_steps > 0).then_some(log_steps);
            info.steps_total = None;
        } else if let Some(dt) = info.delta_t {
            if dt > 0.0 {
                info.steps_done = Some((elapsed / dt) as u64);
                info.steps_total = Some((duration / dt) as u64);
            }
        }
    }

    info
}

/// Print a table row for one job (compact format, used by `progress` without args).
pub fn print_row(info: &ProgressInfo) {
    let pct = info
        .progress_pct
        .map(|p| format!("{:5.1}%", p))
        .unwrap_or_else(|| "   N/A".into());

    let time_range = match (info.current_time, info.end_time) {
        (Some(c), Some(e)) => format!("{:.4}/{:.4}", c, e),
        (Some(c), None) => format!("{:.4}/?", c),
        _ => "?/?".into(),
    };

    let steps = match (info.steps_done, info.steps_total) {
        (Some(d), Some(t)) => format!("{}/{}", d, t),
        (Some(d), None) => format!("{}/?", d),
        _ => "N/A".into(),
    };

    let eta = info
        .eta_seconds
        .map(|e| format!("剩余≈{}", format_duration(e)))
        .unwrap_or_else(|| "N/A".into());

    let step_speed = match info.steps_done {
        Some(d) if d > 0 && info.wall_elapsed > 0.0 => {
            let avg = info.wall_elapsed / d as f64;
            format!("{:.1}s/步", avg)
        }
        _ => "N/A".into(),
    };

    println!(
        "  {:<4} {:<8} {:<8} {:<16} {:<12} {:<18} {:<20} {}",
        info.jid,
        info.core_label,
        pct,
        time_range,
        steps,
        eta,
        info.phase.as_deref().unwrap_or("-"),
        step_speed,
    );
}

/// Print a detailed view for a single job.
pub fn print_detail(info: &ProgressInfo) {
    println!(
        "作业 {}  核心 {}  已运行 {}",
        info.jid,
        info.core_label,
        format_duration(info.wall_elapsed)
    );

    if let Some(e) = &info.error {
        println!("  错误: {}", e);
        return;
    }

    if let Some(phase) = &info.phase {
        println!("  当前阶段:  {}", phase);
    }

    match (info.current_time, info.end_time) {
        (Some(c), Some(e)) => {
            let pct = info.progress_pct.unwrap_or(0.0);
            println!("  仿真时间:  {:.4} / {:.4}  ({:.1}%)", c, e, pct);
            if let Some(s) = info.start_time {
                println!("  阶段起点:  {:.4}", s);
            }
        }
        (Some(c), None) => {
            println!("  仿真时间:  {:.4} / ?", c);
        }
        _ => {
            println!("  仿真时间:  (等待日志输出)");
        }
    }

    match (info.steps_done, info.steps_total, info.delta_t) {
        (Some(d), Some(t), Some(dt)) => {
            println!("  时间步:    {} / {}  (deltaT={})", d, t, dt);
        }
        (Some(d), None, _) if info.adaptive_time_step => {
            println!("  时间步:    {} / ?  (自适应)", d);
        }
        (Some(d), _, Some(dt)) => {
            println!("  时间步:    {} / ?  (deltaT={})", d, dt);
        }
        _ => {}
    }

    if let Some(d) = info.steps_done {
        if d > 0 && info.wall_elapsed > 0.0 {
            let avg = info.wall_elapsed / d as f64;
            println!("  平均步速:  {:.1}s/步", avg);
        }
    }

    if let Some(eta) = info.eta_seconds {
        println!("  预计剩余:  {}", format_duration(eta));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        println!("  预计完成:  {}", format_time_of_day(now + eta));
    }

    if let Some(dir) = &info.case_dir {
        println!("  Case:      {}", dir.display());
    }
}

/// Print header for the compact table.
pub fn print_table_header() {
    println!(
        "{:^6} {:^8} {:^8} {:^16} {:^12} {:^18} {:^20} {}",
        "作业", "核心", "进度", "当前/总时间", "步数", "ETA", "阶段", "步速"
    );
    println!("{}", "-".repeat(88));
}
