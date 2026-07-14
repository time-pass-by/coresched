/// CPU scheduling engine.
///
/// Allocation policies with per-priority exclusivity:
/// - P3: exclusive access to the last physical core, can also use all others
/// - P2: exclusive access to the second-to-last core, can also use shared pool
/// - P1/P0: only use shared pool (excluded from the last 2 cores)
/// - `cfd`: allocate whole physical cores (both hyperthreads)
/// - `py`: allocate individual logical CPUs
use crate::state::save_to_disk;
use crate::state::{CoreschedState, JobEntry, JobPriority};
use crate::topology;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::time::SystemTime;

/// The physical core that `priority` has EXCLUSIVE access to (only that
/// priority level can use it). Returns None if the machine is too small.
pub fn priority_exclusive_core(priority: JobPriority) -> Option<u8> {
    let total = topology::num_physical_cores() as u8;
    match priority {
        JobPriority::P3 if total >= 1 => Some(total - 1),
        JobPriority::P2 if total >= 2 => Some(total - 2),
        _ => None,
    }
}

/// Physical cores forbidden for `priority` (reserved by strictly higher
/// levels).
fn forbidden_cores(priority: JobPriority) -> Vec<u8> {
    match priority {
        JobPriority::P3 => vec![],
        JobPriority::P2 => priority_exclusive_core(JobPriority::P3)
            .into_iter()
            .collect(),
        _ => [
            JobPriority::P3,
            JobPriority::P2,
        ]
        .iter()
        .filter_map(|&p| priority_exclusive_core(p))
        .collect(),
    }
}

/// Logical CPUs forbidden for `priority` (belonging to cores reserved by
/// strictly higher levels).
fn forbidden_cpus(priority: JobPriority) -> Vec<u8> {
    let mut cpus = Vec::new();
    for core in forbidden_cores(priority) {
        if let Some((a, b)) = topology::logical_cpus(core) {
            cpus.push(a);
            if b != a {
                cpus.push(b);
            }
        }
    }
    cpus
}

/// CPUs a job priority is allowed to use, with per-priority exclusivity.
pub fn eligible_cpus(_state: &CoreschedState, priority: JobPriority) -> Vec<u8> {
    let mut cpus = Vec::new();

    // Own exclusive core's CPUs first
    if let Some(core) = priority_exclusive_core(priority) {
        if let Some((first, second)) = topology::logical_cpus(core) {
            cpus.push(first);
            if second != first {
                cpus.push(second);
            }
        }
    }

    // Then shared pool (exclude higher-priority-exclusive CPUs)
    let forbidden = forbidden_cpus(priority);
    for cpu in topology::schedulable_cpus() {
        if !forbidden.contains(&cpu) && !cpus.contains(&cpu) {
            cpus.push(cpu);
        }
    }

    cpus
}

/// Physical cores a job priority is allowed to use, with per-priority
/// exclusivity.
pub fn eligible_physical_cores(_state: &CoreschedState, priority: JobPriority) -> Vec<u8> {
    let mut cores = Vec::new();

    // Own exclusive core first
    if let Some(core) = priority_exclusive_core(priority) {
        cores.push(core);
    }

    // Then shared pool (exclude higher-priority-exclusive cores)
    let forbidden = forbidden_cores(priority);
    for core in topology::physical_cores() {
        if !forbidden.contains(&core) && !cores.contains(&core) {
            cores.push(core);
        }
    }

    cores
}

pub fn reserved_physical_cores(state: &CoreschedState) -> Vec<u8> {
    state
        .reserved_cores()
        .into_iter()
        .filter(|&core| topology::logical_cpus(core).is_some())
        .collect()
}

fn physical_core_is_free(state: &CoreschedState, core: u8) -> bool {
    if state.cfd.get(&core.to_string()).copied().unwrap_or(false) {
        return false;
    }

    topology::logical_cpus(core)
        .map(|(first, second)| {
            state
                .cpus
                .get(&first.to_string())
                .copied()
                .unwrap_or(None)
                .is_none()
                && state
                    .cpus
                    .get(&second.to_string())
                    .copied()
                    .unwrap_or(None)
                    .is_none()
        })
        .unwrap_or(false)
}

/// Allocate N physical cores for a job with the requested priority.
/// Returns the list of physical core indices assigned.
pub fn alloc_cfd_for_priority(state: &mut CoreschedState, n: u8, priority: JobPriority) -> Vec<u8> {
    let mut got = Vec::new();
    for c in eligible_physical_cores(state, priority) {
        if got.len() >= n as usize {
            break;
        }
        if physical_core_is_free(state, c) {
            // Mark physical core as used
            state.cfd.insert(c.to_string(), true);
            // Also mark both logical CPUs
            if let Some((a, b)) = topology::logical_cpus(c) {
                state.cpus.insert(a.to_string(), Some(true));
                state.cpus.insert(b.to_string(), Some(true));
            }
            got.push(c);
        }
    }
    got
}

/// Allocate N logical CPUs for a Python job with the requested priority.
/// Returns the list of logical CPU numbers assigned.
pub fn alloc_py_for_priority(state: &mut CoreschedState, n: u8, priority: JobPriority) -> Vec<u8> {
    let mut got = Vec::new();
    for c in eligible_cpus(state, priority) {
        if got.len() >= n as usize {
            break;
        }
        if state
            .cpus
            .get(&c.to_string())
            .copied()
            .unwrap_or(None)
            .is_none()
        {
            state.cpus.insert(c.to_string(), Some(true));
            got.push(c);
        }
    }
    got
}

/// Free resources held by a job. Removes its entry from `pids`.
pub fn free_job(state: &mut CoreschedState, jid: &str) {
    let entry = match state.pids.remove(jid) {
        Some(e) => e,
        None => return,
    };

    // Free CFD cores + their hyperthreads
    if let Some(cores) = &entry.cores {
        for &c in cores {
            state.cfd.insert(c.to_string(), false);
            if let Some((a, b)) = topology::logical_cpus(c) {
                state.cpus.insert(a.to_string(), None);
                state.cpus.insert(b.to_string(), None);
            }
        }
    }

    // Free individual logical CPUs
    if let Some(cpus) = &entry.cpus {
        for &c in cpus {
            state.cpus.insert(c.to_string(), None);
        }
    }
}

/// Remove entries for dead processes (orphan cleanup).
/// Checks liveness via kill(pid, 0): ESRCH→dead, EPERM→preserve (foreign user),
/// other errors→preserve (can't determine).
/// Returns the number of cleaned-up entries.
pub fn orphan_cleanup(state: &mut CoreschedState) -> usize {
    let mut cleaned = 0;

    let dead_pids: Vec<String> = state
        .pids
        .iter()
        .filter_map(|(jid, entry)| {
            entry.pid.and_then(|pid| {
                match kill(Pid::from_raw(pid as i32), None) {
                    Ok(_) => None, // process still alive
                    Err(nix::errno::Errno::ESRCH) => Some(jid.clone()),
                    Err(_) => None, // permission issue, leave it
                }
            })
        })
        .collect();

    for jid in &dead_pids {
        free_job(state, jid);
        cleaned += 1;
    }

    // Clean pending entries too
    let dead_pending: Vec<String> = state
        .pending
        .iter()
        .filter_map(|(jid, entry)| {
            entry
                .pid
                .and_then(|pid| match kill(Pid::from_raw(pid as i32), None) {
                    Ok(_) => None,
                    Err(nix::errno::Errno::ESRCH) => Some(jid.clone()),
                    Err(_) => None,
                })
        })
        .collect();

    for jid in &dead_pending {
        state.pending.remove(jid);
        cleaned += 1;
    }

    cleaned
}

/// Check how many more resources of a type are needed.
pub fn needed_for_priority(
    state: &CoreschedState,
    job_type: &str,
    n: u8,
    priority: JobPriority,
) -> u8 {
    match job_type {
        "cfd" => {
            let available = eligible_physical_cores(state, priority)
                .into_iter()
                .filter(|&core| physical_core_is_free(state, core))
                .count() as u8;
            n.saturating_sub(available)
        }
        "py" => {
            let available = eligible_cpus(state, priority)
                .into_iter()
                .filter(|cpu| {
                    state
                        .cpus
                        .get(&cpu.to_string())
                        .copied()
                        .unwrap_or(None)
                        .is_none()
                })
                .count() as u8;
            n.saturating_sub(available)
        }
        _ => n,
    }
}

/// Whether a higher-priority direct `run` job is waiting for allocation.
pub fn has_higher_priority_pending(
    state: &CoreschedState,
    priority: JobPriority,
    exclude_jid: Option<&str>,
) -> bool {
    state
        .pending
        .iter()
        .any(|(jid, entry)| entry.priority > priority && Some(jid.as_str()) != exclude_jid)
}

/// Whether a direct `run` job must yield to waiting work at a higher level.
pub fn has_higher_priority_waiting(
    state: &CoreschedState,
    priority: JobPriority,
    exclude_pending_jid: Option<&str>,
) -> bool {
    has_higher_priority_pending(state, priority, exclude_pending_jid)
        || state.queued.iter().any(|entry| entry.priority > priority)
}

/// Return the next queue entry under strict 3 -> 2 -> 1 -> 0 ordering. Jobs retain
/// FIFO order within a level and yield to a waiting direct `run` at a higher level.
pub fn next_queued_job_index(state: &CoreschedState) -> Option<usize> {
    for priority in [JobPriority::P3, JobPriority::P2, JobPriority::P1, JobPriority::P0] {
        if let Some(index) = state
            .queued
            .iter()
            .position(|entry| entry.priority == priority)
        {
            if has_higher_priority_pending(state, priority, None) {
                return None;
            }
            return Some(index);
        }
    }

    None
}

/// Info needed by the parent to spawn a dispatched child.
#[derive(Debug, Clone)]
pub struct JobDispatchInfo {
    pub jid: String,
    pub job_type: String,
    pub priority: JobPriority,
    pub need: u8,
    pub cmd: String,
    pub args: Vec<String>,
    pub output: String,
    pub submit: Option<f64>,
    pub pin_cpus: Vec<u8>,
}

/// Try to select the next priority-eligible queued job, allocate resources,
/// and move it into `pids`. Returns `None` when the queue is empty or the
/// highest priority waiting job cannot yet be allocated.
pub fn try_dispatch_one(state: &mut CoreschedState) -> Option<JobDispatchInfo> {
    let index = next_queued_job_index(state)?;
    let job = state.queued.get(index)?;

    let need = needed_for_priority(state, &job.job_type, job.need as u8, job.priority);
    if need > 0 {
        return None;
    }

    let job = state.queued.remove(index).unwrap();
    let jid = job.jid.clone();
    let jt = job.job_type.clone();
    let priority = job.priority;
    let n = job.need as u8;
    let cmd = job.cmd.clone().unwrap_or_default();
    let output = job.output.clone().unwrap_or_default();
    let submit = job.submit;

    let args: Vec<String> = serde_json::from_str(&cmd).unwrap_or_else(|_| vec![cmd.clone()]);

    let cores = match jt.as_str() {
        "cfd" => alloc_cfd_for_priority(state, n, priority),
        "py" => alloc_py_for_priority(state, n, priority),
        _ => return None,
    };

    let pin_cpus: Vec<u8> = match jt.as_str() {
        "cfd" => cores
            .iter()
            .flat_map(|&c| {
                let (a, b) = topology::logical_cpus(c).unwrap();
                vec![a, b]
            })
            .collect(),
        "py" => cores.clone(),
        _ => vec![],
    };

    let start_ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let entry = JobEntry {
        jid: jid.clone(),
        job_type: jt.clone(),
        need: n as u64,
        pid: None,
        submit,
        start: Some(start_ts),
        cmd: Some(cmd.clone()),
        output: Some(output.clone()),
        cores: if jt == "cfd" { Some(cores) } else { None },
        cpus: if jt == "py" {
            Some(pin_cpus.clone())
        } else {
            None
        },
        priority,
    };

    state.pids.insert(jid.clone(), entry);
    save_to_disk(state).ok();

    Some(JobDispatchInfo {
        jid,
        job_type: jt,
        priority,
        need: n,
        cmd,
        args,
        output,
        submit,
        pin_cpus,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CoreschedState;

    fn fresh_state() -> CoreschedState {
        CoreschedState::default()
    }

    #[test]
    fn test_alloc_cfd_basic() {
        let mut s = fresh_state();
        let cores = alloc_cfd_for_priority(&mut s, 2, JobPriority::P1);
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[0], 0);
        assert_eq!(cores[1], 1);
        assert!(s.cfd["0"]);
        assert!(s.cpus["0"].unwrap());
        assert!(s.cpus["8"].unwrap());
    }

    #[test]
    fn test_alloc_cfd_exhausted_p2() {
        let mut s = fresh_state();
        // P2 cannot use the last core (P3-exclusive), so only 7 cores.
        let cores = alloc_cfd_for_priority(&mut s, 10, JobPriority::P2);
        assert_eq!(cores.len(), 7);
    }

    #[test]
    fn test_alloc_cfd_exhausted_p3() {
        let mut s = fresh_state();
        let cores = alloc_cfd_for_priority(&mut s, 10, JobPriority::P3);
        assert_eq!(cores.len(), 8); // P3 can use all
    }

    #[test]
    fn test_alloc_py_basic() {
        let mut s = fresh_state();
        let cpus = alloc_py_for_priority(&mut s, 4, JobPriority::P1);
        assert_eq!(cpus.len(), 4);
        assert_eq!(cpus, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_cfd_then_py_no_overlap() {
        let mut s = fresh_state();
        alloc_cfd_for_priority(&mut s, 1, JobPriority::P1);
        let py = alloc_py_for_priority(&mut s, 2, JobPriority::P1);
        assert_eq!(py, vec![1, 2]);
    }

    #[test]
    fn test_free_and_reuse() {
        let mut s = fresh_state();
        let cores = alloc_cfd_for_priority(&mut s, 1, JobPriority::P1);
        let jid = "j0001".to_string();
        s.pids.insert(
            jid.clone(),
            crate::state::JobEntry {
                jid: jid.clone(),
                job_type: "cfd".into(),
                need: 1,
                pid: None,
                submit: None,
                start: None,
                cmd: None,
                output: None,
                cores: Some(cores),
                cpus: None,
                priority: JobPriority::P1,
            },
        );

        free_job(&mut s, &jid);
        assert!(!s.cfd["0"]);
        assert!(s.cpus["0"].is_none());
        assert!(s.cpus["8"].is_none());
    }

    #[test]
    fn test_needed_for() {
        let mut s = fresh_state();
        alloc_cfd_for_priority(&mut s, 6, JobPriority::P2);
        // P2 eligible: core 6 first, then [0..5] = 7 total.
        // 6 allocated → 1 free (core 5).
        assert_eq!(needed_for_priority(&s, "cfd", 8, JobPriority::P2), 7);
        assert_eq!(needed_for_priority(&s, "cfd", 4, JobPriority::P2), 3);
        assert_eq!(needed_for_priority(&s, "cfd", 2, JobPriority::P2), 1);
        assert_eq!(needed_for_priority(&s, "cfd", 1, JobPriority::P2), 0);
    }

    #[test]
    fn test_reserved_cores_are_excluded_from_priority_zero_and_one_jobs() {
        let mut s = fresh_state();

        // P0/P1 cannot use core 6 (P2) or core 7 (P3).
        assert_eq!(
            eligible_physical_cores(&s, JobPriority::P1),
            vec![0, 1, 2, 3, 4, 5]
        );
        // P0/P1 py limit on 8-core: 12 CPUs (0-5, 8-13)
        assert_eq!(needed_for_priority(&s, "py", 12, JobPriority::P1), 0);

        let priority_one_cpus = alloc_py_for_priority(&mut s, 12, JobPriority::P1);
        assert_eq!(priority_one_cpus.len(), 12);
        assert!(!priority_one_cpus.contains(&6));
        assert!(!priority_one_cpus.contains(&7));
        assert!(!priority_one_cpus.contains(&14));
        assert!(!priority_one_cpus.contains(&15));

        let mut s = fresh_state();
        let priority_zero_cpus = alloc_py_for_priority(&mut s, 12, JobPriority::P0);
        assert!(!priority_zero_cpus.contains(&6));
        assert!(!priority_zero_cpus.contains(&7));
        assert!(!priority_zero_cpus.contains(&14));
        assert!(!priority_zero_cpus.contains(&15));
    }

    #[test]
    fn test_p2_prefers_own_exclusive_core_only() {
        let mut s = fresh_state();
        // P2 gets core 6 first (its exclusive core), NOT core 7 (P3's).
        assert_eq!(
            alloc_cfd_for_priority(&mut s, 2, JobPriority::P2),
            vec![6, 0]
        );

        let mut s = fresh_state();
        // P2 py: get CPUs of core 6 first (6, 14), then shared pool.
        assert_eq!(
            alloc_py_for_priority(&mut s, 4, JobPriority::P2),
            vec![6, 14, 0, 1]
        );
    }

    #[test]
    fn test_p3_gets_last_core_exclusively() {
        let mut s = fresh_state();
        // P3 cfd: gets core 7 first (its exclusive core).
        assert_eq!(
            alloc_cfd_for_priority(&mut s, 2, JobPriority::P3),
            vec![7, 0]
        );

        let mut s = fresh_state();
        // P3 py: gets CPUs of core 7 first (7, 15), then shared pool.
        assert_eq!(
            alloc_py_for_priority(&mut s, 4, JobPriority::P3),
            vec![7, 15, 0, 1]
        );
    }

    #[test]
    fn test_p3_no_interference_from_p2() {
        // P3 has access to all 8 cores. P2 sees only 7.
        let s = fresh_state();
        let p3_eligible = eligible_physical_cores(&s, JobPriority::P3);
        let p2_eligible = eligible_physical_cores(&s, JobPriority::P2);
        assert_eq!(p3_eligible.len(), 8);
        assert_eq!(p2_eligible.len(), 7);
        assert!(!p2_eligible.contains(&(topology::num_physical_cores() as u8 - 1)));
    }

    #[test]
    fn test_cfd_does_not_overlap_existing_python_allocation() {
        let mut s = fresh_state();
        s.cpus.insert("0".to_string(), Some(true));
        let cores = alloc_cfd_for_priority(&mut s, 1, JobPriority::P1);
        assert_eq!(cores, vec![1]);
    }

    fn queued_job(jid: &str, priority: JobPriority) -> JobEntry {
        JobEntry {
            jid: jid.to_string(),
            job_type: "py".to_string(),
            need: 1,
            pid: None,
            submit: None,
            start: None,
            cmd: None,
            output: None,
            cores: None,
            cpus: None,
            priority,
        }
    }

    #[test]
    fn test_queued_priority_order_p3_first() {
        let mut s = fresh_state();
        s.queued.push_back(queued_job("j0001", JobPriority::P0));
        s.queued.push_back(queued_job("j0002", JobPriority::P1));
        s.queued.push_back(queued_job("j0003", JobPriority::P2));
        s.queued.push_back(queued_job("j0004", JobPriority::P3));
        assert_eq!(next_queued_job_index(&s), Some(3)); // P3 first
    }

    #[test]
    fn test_waiting_higher_priority_run_blocks_lower_queue_dispatch() {
        let mut s = fresh_state();
        s.pending
            .insert("j0001".to_string(), queued_job("j0001", JobPriority::P3));
        s.queued.push_back(queued_job("j0002", JobPriority::P2));
        assert_eq!(next_queued_job_index(&s), None);
    }

    #[test]
    fn test_forbidden_cores() {
        let total = topology::num_physical_cores() as u8;
        // P3: nothing forbidden
        assert!(forbidden_cores(JobPriority::P3).is_empty());
        // P2: P3's exclusive core forbidden
        if total >= 1 {
            assert_eq!(forbidden_cores(JobPriority::P2), vec![total - 1]);
        }
        // P0/P1: both P3's and P2's exclusive cores forbidden
        if total >= 2 {
            let mut f = forbidden_cores(JobPriority::P0);
            f.sort();
            assert_eq!(f, vec![total - 2, total - 1]);
        }
    }
}
