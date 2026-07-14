/// CPU affinity binding — direct `sched_setaffinity` syscall via `nix`.
///
/// Replaces the original Python version's `taskset -c` external process call.
use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;

/// Pin the current process (and all its threads) to the given set of logical CPUs.
pub fn pin_to_cpus(cpus: &[u8]) -> Result<(), nix::Error> {
    let mut set = CpuSet::new();
    for &c in cpus {
        set.set(c as usize)?;
    }
    sched_setaffinity(Pid::from_raw(0), &set) // 0 = current process
}

/// Return a comma-separated CPU list string (same format as `taskset -c`).
pub fn pin_str(cpus: &[u8]) -> String {
    cpus.iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pin_str() {
        assert_eq!(pin_str(&[0, 1, 2]), "0,1,2");
        assert_eq!(pin_str(&[7]), "7");
    }

    #[test]
    fn test_cpuset_operations() {
        let mut set = CpuSet::new();
        assert!(set.set(0).is_ok());
        assert!(set.set(7).is_ok());
        assert!(set.is_set(0).unwrap());
        assert!(set.is_set(7).unwrap());
        assert!(!set.is_set(1).unwrap());
    }
}
