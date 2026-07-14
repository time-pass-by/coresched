/// CPU topology — auto-detected at startup from /sys/devices/system/cpu.
///
/// Groups logical CPUs by (package, core_id) to discover physical cores
/// and SMT siblings. The result is cached in a `OnceLock` so every call
/// is cheap after the first access.
///
/// Fallback: when /sys is absent the topology assumes one physical core
/// per logical CPU (no SMT), reading the CPU count from the OS.
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct TopologyData {
    pub physical_cores: Vec<u8>,
    pub all_cpus: Vec<u8>,
    pub siblings: Vec<(u8, u8)>,
    pub schedulable_cpus: Vec<u8>,
}

static TOPOLOGY: OnceLock<TopologyData> = OnceLock::new();

fn get() -> &'static TopologyData {
    TOPOLOGY.get_or_init(detect_topology)
}

/// Search /sys/devices/system/cpu for per-CPU topology files.
fn detect_topology() -> TopologyData {
    let base = "/sys/devices/system/cpu";

    // Collect all cpuN directories.
    let mut cpus: Vec<u8> = match std::fs::read_dir(base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("cpu") {
                    name[3..].parse::<u8>().ok()
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    cpus.sort_unstable();

    if cpus.is_empty() {
        return cpu_count_fallback();
    }

    // Group by (package_id, core_id).
    let mut groups: std::collections::BTreeMap<(u32, u32), Vec<u8>> =
        std::collections::BTreeMap::new();

    for &cpu in &cpus {
        let pkg_path = format!("{base}/cpu{cpu}/topology/physical_package_id");
        let core_path = format!("{base}/cpu{cpu}/topology/core_id");
        let pkg = read_u32(&pkg_path).unwrap_or(0);
        let core_id = match read_u32(&core_path) {
            Some(id) => id,
            None => {
                // Missing core_id → treat each CPU as its own core.
                groups.insert((cpu as u32, pkg), vec![cpu]);
                continue;
            }
        };
        groups.entry((pkg, core_id)).or_default().push(cpu);
    }

    let physical_cores: Vec<u8> = (0..groups.len() as u8).collect();
    let siblings: Vec<(u8, u8)> = groups
        .values()
        .map(|sibs| {
            let mut v = sibs.clone();
            v.sort_unstable();
            v.dedup();
            (v.first().copied().unwrap_or(0), v.get(1).copied().unwrap_or(v[0]))
        })
        .collect();
    let all_cpus = cpus;
    let schedulable_cpus = all_cpus.clone();

    TopologyData {
        physical_cores,
        all_cpus,
        siblings,
        schedulable_cpus,
    }
}

fn read_u32(path: &str) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Fallback when /sys is unavailable: read the CPU count and assume no SMT.
fn cpu_count_fallback() -> TopologyData {
    let n: u8 = std::thread::available_parallelism()
        .map(|p| p.get() as u8)
        .unwrap_or(1)
        .min(255);
    let physical_cores: Vec<u8> = (0..n).collect();
    let siblings: Vec<(u8, u8)> = (0..n).map(|c| (c, c)).collect();
    let all_cpus: Vec<u8> = (0..n).collect();
    TopologyData {
        physical_cores,
        all_cpus: all_cpus.clone(),
        siblings,
        schedulable_cpus: all_cpus,
    }
}

// ---------------------------------------------------------------------------
// Public API — identical signatures to the original hardcoded module.
// ---------------------------------------------------------------------------

/// Return the two logical CPUs belonging to physical core `physical`.
pub fn logical_cpus(physical: u8) -> Option<(u8, u8)> {
    get().siblings.get(physical as usize).copied()
}

/// Iterate over every physical core index.
pub fn physical_cores() -> impl Iterator<Item = u8> {
    get().physical_cores.clone().into_iter()
}

/// Iterate over every logical CPU index.
pub fn all_cpus() -> impl Iterator<Item = u8> {
    get().all_cpus.clone().into_iter()
}

/// Logical CPUs eligible for `--cpus` jobs (currently all).
pub fn schedulable_cpus() -> impl Iterator<Item = u8> {
    get().schedulable_cpus.clone().into_iter()
}

/// Whether a logical CPU may be assigned to a regular `--cpus` job.
pub fn is_schedulable_cpu(cpu: u8) -> bool {
    get().schedulable_cpus.contains(&cpu)
}

/// Total number of physical cores.
pub fn num_physical_cores() -> usize {
    get().physical_cores.len()
}

/// Total number of logical CPUs.
pub fn num_logical_cpus() -> usize {
    get().all_cpus.len()
}

/// Reserve fraction of cores by default: the last max(0, ⌈N/4⌉) physical cores
/// when N >= 4, otherwise none.
pub fn default_reserved_cores() -> Vec<u8> {
    let n = num_physical_cores();
    if n < 4 {
        return Vec::new();
    }
    let keep = ((n + 3) / 4).max(1).min(n);
    ((n - keep) as u8..n as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topology_is_consistent() {
        let phy: Vec<u8> = physical_cores().collect();
        let cpu: Vec<u8> = all_cpus().collect();
        assert!(!phy.is_empty());
        assert!(!cpu.is_empty());
        assert_eq!(phy.len(), siblings_count());
    }

    fn siblings_count() -> usize {
        let mut i = 0;
        while logical_cpus(i as u8).is_some() {
            i += 1;
        }
        i
    }

    #[test]
    fn test_logical_cpus_present_on_this_machine() {
        let p0 = logical_cpus(0);
        assert!(p0.is_some());
        let (first, second) = p0.unwrap();
        assert!(first < second || first == second); // second >= first
    }

    #[test]
    fn test_physical_cores_count_matches_siblings() {
        let phy: Vec<_> = physical_cores().collect();
        let mut from_siblings = 0;
        while logical_cpus(from_siblings).is_some() {
            from_siblings += 1;
        }
        assert_eq!(phy.len(), from_siblings as usize);
    }

    #[test]
    fn test_default_reserved_cores_reasonable() {
        let reserved = default_reserved_cores();
        let total = num_physical_cores();
        if total >= 4 {
            assert!(!reserved.is_empty());
            assert!(reserved.len() <= total);
        }
    }
}
