/// CPU topology: 8 physical cores, each with 2 hyperthreads (logical CPUs).
///
/// Mapping (matching the original Python version):
///   Physical core 0 → logical CPUs (0,  8)
///   Physical core 1 → logical CPUs (1,  9)
///   …
///   Physical core 7 → logical CPUs (7, 15)

const TOPOLOGY: &[(u8, u8); 8] = &[
    (0, 8),
    (1, 9),
    (2, 10),
    (3, 11),
    (4, 12),
    (5, 13),
    (6, 14),
    (7, 15),
];

/// All physical core indices (0..8).
pub const PHYSICAL: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7];

/// All logical CPU indices (0..16).
pub const ALL_CPUS: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Logical CPUs available to regular `--cpus` jobs before priority reservations
/// are applied.
///
/// All hardware threads are schedulable. The priority reservation policy then
/// removes complete physical-core pairs from priority 0 and 1 allocations.
pub const SCHEDULABLE_CPUS: &[u8] = ALL_CPUS;

/// Return the two logical CPUs belonging to a physical core.
pub fn logical_cpus(physical: u8) -> Option<(u8, u8)> {
    TOPOLOGY.get(physical as usize).copied()
}

/// Return all physical core numbers.
pub fn physical_cores() -> impl Iterator<Item = u8> {
    PHYSICAL.iter().copied()
}

/// Return all logical CPU numbers.
pub fn all_cpus() -> impl Iterator<Item = u8> {
    ALL_CPUS.iter().copied()
}

/// Return logical CPUs that may be assigned to regular `--cpus` jobs.
pub fn schedulable_cpus() -> impl Iterator<Item = u8> {
    SCHEDULABLE_CPUS.iter().copied()
}

/// Whether a logical CPU may be assigned to regular `--cpus` jobs.
pub fn is_schedulable_cpu(cpu: u8) -> bool {
    SCHEDULABLE_CPUS.contains(&cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topology_mapping() {
        assert_eq!(logical_cpus(0), Some((0, 8)));
        assert_eq!(logical_cpus(7), Some((7, 15)));
        assert_eq!(logical_cpus(8), None);
    }

    #[test]
    fn test_iterators() {
        assert_eq!(physical_cores().count(), 8);
        assert_eq!(all_cpus().count(), 16);
        assert_eq!(schedulable_cpus().count(), 16);
    }
}
