/// State persistence module.
///
/// Manages `~/.coresched/state.json` with file locking for concurrency.
/// Fully backward-compatible with the original Python version's JSON format.
use fd_lock::RwLock;
use serde::de::{Deserializer, Error as DeError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Data structures — match the Python version's schema exactly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub enum JobPriority {
    #[serde(rename = "0", alias = "low", alias = "normal")]
    P0,
    #[serde(rename = "1")]
    P1,
    #[serde(rename = "2", alias = "high")]
    P2,
}

pub fn default_reserved_physical_cores() -> Vec<u8> {
    crate::topology::default_reserved_cores()
}

fn normalize_reserved_physical_cores(cores: Vec<u8>) -> Vec<u8> {
    let max_core = crate::topology::num_physical_cores() as u8;
    let mut normalized = Vec::new();
    for core in cores {
        if core < max_core && !normalized.contains(&core) {
            normalized.push(core);
        }
    }
    normalized
}

fn deserialize_reserved_physical_cores<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let cores = match value {
        serde_json::Value::Null => return Ok(default_reserved_physical_cores()),
        serde_json::Value::Number(number) => vec![number
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| D::Error::custom("reserved physical core must be an integer"))?],
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| D::Error::custom("reserved physical cores must be integers"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(D::Error::custom(
                "reserved physical cores must be an integer or an array of integers",
            ));
        }
    };

    Ok(normalize_reserved_physical_cores(cores))
}

impl Default for JobPriority {
    fn default() -> Self {
        Self::P0
    }
}

impl JobPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "0",
            Self::P1 => "1",
            Self::P2 => "2",
        }
    }
}

impl fmt::Display for JobPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobPriority {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "0" => Ok(Self::P0),
            "1" => Ok(Self::P1),
            "2" => Ok(Self::P2),
            _ => Err("priority must be 0, 1, or 2".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct JobEntry {
    pub jid: String,
    #[serde(rename = "type")]
    pub job_type: String, // "cfd" | "py"
    pub need: u64,
    pub pid: Option<u32>,
    pub submit: Option<f64>, // unix timestamp
    pub start: Option<f64>,
    pub cmd: Option<String>,
    pub output: Option<String>,
    pub cores: Option<Vec<u8>>,
    pub cpus: Option<Vec<u8>>,
    #[serde(default)]
    pub priority: JobPriority,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreschedState {
    #[serde(default)]
    pub cfd: HashMap<String, bool>, // "0".."7" → allocated?
    #[serde(default)]
    pub cpus: HashMap<String, Option<bool>>, // "0".."15" → Some(true)=used, None=free
    #[serde(default)]
    pub pids: HashMap<String, JobEntry>,
    #[serde(default)]
    pub pending: HashMap<String, JobEntry>,
    #[serde(default)]
    pub queued: VecDeque<JobEntry>,
    /// Physical cores excluded from priority 0 and 1 jobs and reserved for 2.
    ///
    /// `reserved_physical_core` was the single-core field used by earlier
    /// versions. Keep it as a serde alias so state files migrate in place.
    #[serde(
        default = "default_reserved_physical_cores",
        alias = "reserved_physical_core",
        deserialize_with = "deserialize_reserved_physical_cores"
    )]
    pub reserved_physical_cores: Vec<u8>,
    #[serde(rename = "next")]
    pub next_jid: u64,
}

impl Default for CoreschedState {
    fn default() -> Self {
        let total_cores = crate::topology::num_physical_cores() as u8;
        let total_cpus = crate::topology::num_logical_cpus() as u8;

        let mut cfd = HashMap::new();
        for c in 0..total_cores {
            cfd.insert(c.to_string(), false);
        }
        let mut cpus = HashMap::new();
        for c in 0..total_cpus {
            cpus.insert(c.to_string(), None);
        }
        Self {
            cfd,
            cpus,
            pids: HashMap::new(),
            pending: HashMap::new(),
            queued: VecDeque::new(),
            reserved_physical_cores: default_reserved_physical_cores(),
            next_jid: 1,
        }
    }
}

impl CoreschedState {
    /// Ensure cfd/cpus maps cover the current topology, filling any gaps.
    pub fn migrate(&mut self) {
        let total_cores = crate::topology::num_physical_cores() as u8;
        let total_cpus = crate::topology::num_logical_cpus() as u8;
        for c in 0..total_cores {
            self.cfd.entry(c.to_string()).or_insert(false);
        }
        for c in 0..total_cpus {
            self.cpus.entry(c.to_string()).or_insert(None);
        }
        self.reserved_physical_cores = normalize_reserved_physical_cores(
            std::mem::take(&mut self.reserved_physical_cores),
        );
    }
    pub fn reserved_cores(&self) -> Vec<u8> {
        normalize_reserved_physical_cores(self.reserved_physical_cores.clone())
    }

    pub fn set_reserved_cores(&mut self, cores: Vec<u8>) {
        self.reserved_physical_cores = normalize_reserved_physical_cores(cores);
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn state_dir() -> PathBuf {
    home_dir().join(".coresched")
}

fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

fn lock_path() -> PathBuf {
    state_dir().join("state.lock")
}

pub fn jobs_dir() -> PathBuf {
    state_dir().join("jobs")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

// ---------------------------------------------------------------------------
// Lock (compatible with Python fcntl.flock)
// ---------------------------------------------------------------------------

/// Run a closure while holding an exclusive file lock.
pub fn with_lock<T>(f: impl FnOnce() -> T) -> io::Result<T> {
    let p = lock_path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&p)?;
    let mut lock = RwLock::new(file);
    let _guard = lock.write();
    Ok(f())
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Initialise state directory and default state if missing.
pub fn init() -> io::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    fs::create_dir_all(jobs_dir())?;

    if !state_path().exists() {
        let s = CoreschedState::default();
        save_to_disk(&s)?;
    }
    Ok(())
}

/// Load state from disk (caller must hold lock).
/// Automatically migrates old state files to the current topology.
pub fn load_from_disk() -> io::Result<CoreschedState> {
    let data = fs::read_to_string(state_path())?;
    let mut s: CoreschedState =
        serde_json::from_str(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    s.migrate();
    Ok(s)
}

/// Save state to disk (caller must hold lock).
pub fn save_to_disk(state: &CoreschedState) -> io::Result<()> {
    let data = serde_json::to_string_pretty(state)?;
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    fs::rename(tmp, path)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a monotonically increasing job ID ("j0001", "j0002", …).
pub fn next_jid(state: &mut CoreschedState) -> String {
    let mut jid = format!("j{:04}", state.next_jid);
    while job_output_path(&jid).exists()
        || state.pids.contains_key(&jid)
        || state.pending.contains_key(&jid)
        || state.queued.iter().any(|entry| entry.jid == jid)
    {
        state.next_jid += 1;
        jid = format!("j{:04}", state.next_jid);
    }
    state.next_jid += 1;
    jid
}

/// Path to a job's output file.
pub fn job_output_path(jid: &str) -> PathBuf {
    jobs_dir().join(format!("{}.out", jid))
}

/// Path a submitted script may atomically update with generic progress JSON.
/// Kept beside the job output so old state files remain schema-compatible.
pub fn job_progress_path(jid: &str) -> PathBuf {
    jobs_dir().join(format!("{}.progress.json", jid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let s = CoreschedState::default();
        let expected_cores = crate::topology::num_physical_cores();
        let expected_cpus = crate::topology::num_logical_cpus();
        assert_eq!(s.cfd.len(), expected_cores);
        assert!(s.cfd.values().all(|v| !v));
        assert_eq!(s.cpus.len(), expected_cpus);
        assert!(s.cpus.values().all(|v| v.is_none()));
        assert_eq!(s.next_jid, 1);
        assert_eq!(
            s.reserved_cores(),
            crate::topology::default_reserved_cores()
        );
    }

    #[test]
    fn test_next_jid() {
        let mut s = CoreschedState::default();
        // Existing job-output files in the user's state directory may reserve
        // low sequence numbers, so only assert the scheduler invariant.
        let first = next_jid(&mut s);
        let second = next_jid(&mut s);
        assert_ne!(first, second);
        assert!(second > first);
        assert!(s.next_jid >= 3);
    }

    #[test]
    fn test_roundtrip_json() {
        let s = CoreschedState::default();
        let json = serde_json::to_string_pretty(&s).unwrap();
        let s2: CoreschedState = serde_json::from_str(&json).unwrap();
        assert_eq!(s.next_jid, s2.next_jid);
        assert_eq!(s.reserved_cores(), s2.reserved_cores());
    }

    #[test]
    fn test_legacy_single_reserved_core_migrates_to_collection() {
        let state: CoreschedState =
            serde_json::from_str(r#"{"reserved_physical_core":7,"next":1}"#).unwrap();
        assert_eq!(state.reserved_cores(), vec![7]);
    }

    #[test]
    fn test_legacy_empty_reservation_adopts_configured_default() {
        let state: CoreschedState =
            serde_json::from_str(r#"{"reserved_physical_core":null,"next":1}"#).unwrap();
        assert_eq!(
            state.reserved_cores(),
            crate::topology::default_reserved_cores()
        );
    }

    #[test]
    fn test_explicit_empty_reservation_stays_cleared() {
        let state: CoreschedState =
            serde_json::from_str(r#"{"reserved_physical_cores":[],"next":1}"#).unwrap();
        assert!(state.reserved_cores().is_empty());
    }

    #[test]
    fn test_legacy_job_defaults_to_priority_zero() {
        let legacy = r#"{
            "jid": "j0001",
            "type": "py",
            "need": 1,
            "pid": null,
            "submit": null,
            "start": null,
            "cmd": null,
            "output": null,
            "cores": null,
            "cpus": null
        }"#;
        let job: JobEntry = serde_json::from_str(legacy).unwrap();
        assert_eq!(job.priority, JobPriority::P0);
    }

    #[test]
    fn test_priority_serializes_as_numeric_and_accepts_previous_labels() {
        assert_eq!(serde_json::to_string(&JobPriority::P2).unwrap(), "\"2\"");
        assert_eq!(
            serde_json::from_str::<JobPriority>("\"normal\"").unwrap(),
            JobPriority::P0
        );
        assert_eq!(
            serde_json::from_str::<JobPriority>("\"high\"").unwrap(),
            JobPriority::P2
        );
    }
}
