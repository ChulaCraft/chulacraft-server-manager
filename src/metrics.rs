//! 5-minute CPU and memory samples, kept 7 days in a JSON file (T2.6), plus the
//! in-memory live state (`since`, `last_restart`, running actions) of each service.
//!
//! The `ServiceBackend` trait has no metrics call, so numbers come from the unit's
//! cgroup files (`cpu.stat`, `memory.current`, `memory.max`), found through the
//! control group that `server_process` already returns. systemd reads the same
//! counters for `CPUUsageNSec`, `MemoryCurrent` and `MemoryMax`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    backend::ServiceBackend,
    config::{Backend, Config, ServiceConfig},
    handlers::AppError,
    state::AppState,
};

pub const DEFAULT_PATH: &str = "/var/lib/mcsv_manager/metrics.json";
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub const BUCKET_SECS: i64 = 300;
pub const RETENTION_SECS: i64 = 7 * 24 * 3600;
/// How long after a restart request a stopping or starting unit reads as `restarting`.
const RESTART_WINDOW_SECS: i64 = 120;

/// `minecraft@<name>.service` gives `<name>`; any other unit is not manageable here.
pub fn instance_of(svc: &ServiceConfig) -> Option<&str> {
    if svc.backend != Backend::Systemd {
        return None;
    }
    svc.unit
        .as_deref()?
        .strip_prefix("minecraft@")?
        .strip_suffix(".service")
}

pub fn bucket(t: i64) -> i64 {
    t - t.rem_euclid(BUCKET_SECS)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Start of the 5-minute bucket, unix seconds.
    pub t: i64,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub up: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UptimePoint {
    pub t: i64,
    pub up_ratio: f64,
}

#[derive(Debug)]
struct Live {
    state: String,
    since: i64,
    last_restart: Option<i64>,
    restart_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    services: HashMap<String, Vec<Sample>>,
}

#[derive(Debug, Default)]
pub struct Store {
    path: Option<PathBuf>,
    samples: Mutex<HashMap<String, Vec<Sample>>>,
    live: Mutex<HashMap<String, Live>>,
    busy: Mutex<HashSet<String>>,
}

/// Marks an action on a service as running until dropped.
pub struct ActionGuard {
    store: Arc<Store>,
    id: String,
}

impl Drop for ActionGuard {
    fn drop(&mut self) {
        self.store.busy.lock().unwrap().remove(&self.id);
    }
}

impl Store {
    /// In memory only (tests).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// A missing or unreadable file starts empty: metrics never block the manager.
    pub fn load(path: &Path) -> Self {
        let samples = match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<File>(&text) {
                Ok(f) => f.services,
                Err(e) => {
                    eprintln!("metrics: ignoring unreadable {}: {e}", path.display());
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                eprintln!("metrics: cannot read {}: {e}", path.display());
                HashMap::new()
            }
        };
        Self {
            path: Some(path.to_path_buf()),
            samples: Mutex::new(samples),
            ..Self::default()
        }
    }

    /// Writes to a temporary file in the same directory, then renames it over.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = serde_json::to_vec(&File {
            services: self.samples.lock().unwrap().clone(),
        })
        .map_err(io::Error::other)?;
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    }

    pub fn record(&self, id: &str, sample: Sample) {
        let mut all = self.samples.lock().unwrap();
        let v = all.entry(id.to_string()).or_default();
        v.retain(|s| s.t != sample.t && s.t >= sample.t - RETENTION_SECS);
        v.push(sample);
        v.sort_by_key(|s| s.t);
    }

    /// Samples of the last `hours`, oldest first. Missing buckets are absent.
    pub fn metrics(&self, id: &str, hours: i64, now: i64) -> Vec<Sample> {
        let from = bucket(now) - hours * 3600;
        self.samples
            .lock()
            .unwrap()
            .get(id)
            .map(|v| v.iter().filter(|s| s.t >= from).cloned().collect())
            .unwrap_or_default()
    }

    pub fn latest(&self, id: &str) -> Option<Sample> {
        self.samples.lock().unwrap().get(id)?.last().cloned()
    }

    /// One point per hour that has samples: the share of them that were `up`.
    /// An hour with no samples (the manager was down) is omitted, not zero-filled.
    pub fn uptime(&self, id: &str, days: i64, now: i64) -> Vec<UptimePoint> {
        let from = now - now.rem_euclid(3600) - (days * 24 - 1) * 3600;
        let mut hours: BTreeMap<i64, (u32, u32)> = BTreeMap::new();
        for s in self
            .samples
            .lock()
            .unwrap()
            .get(id)
            .into_iter()
            .flatten()
            .filter(|s| s.t >= from)
        {
            let e = hours.entry(s.t - s.t.rem_euclid(3600)).or_default();
            e.0 += u32::from(s.up);
            e.1 += 1;
        }
        hours
            .into_iter()
            .map(|(t, (up, total))| UptimePoint {
                t,
                up_ratio: f64::from(up) / f64::from(total),
            })
            .collect()
    }

    /// Records what was just seen and returns `(since, last_restart)`.
    /// `first_since` (the process start time) is used only the first time a service is seen.
    pub fn observe(
        &self,
        id: &str,
        state: &str,
        now: i64,
        first_since: Option<i64>,
    ) -> (i64, Option<i64>) {
        let mut live = self.live.lock().unwrap();
        let e = live.entry(id.to_string()).or_insert_with(|| Live {
            state: state.to_string(),
            since: first_since.unwrap_or(now),
            last_restart: None,
            restart_at: None,
        });
        if e.state != state {
            if state == "up" {
                e.last_restart = Some(now);
            }
            e.state = state.to_string();
            e.since = now;
        }
        (e.since, e.last_restart)
    }

    pub fn mark_restart(&self, id: &str, now: i64) {
        let mut live = self.live.lock().unwrap();
        let e = live.entry(id.to_string()).or_insert_with(|| Live {
            state: String::new(),
            since: now,
            last_restart: None,
            restart_at: None,
        });
        e.restart_at = Some(now);
        e.last_restart = Some(now);
    }

    pub fn restart_recent(&self, id: &str, now: i64) -> bool {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .and_then(|e| e.restart_at)
            .is_some_and(|t| now - t < RESTART_WINDOW_SECS)
    }

    /// `None` when an action on this service is already running.
    pub fn begin_action(self: &Arc<Self>, id: &str) -> Option<ActionGuard> {
        self.busy
            .lock()
            .unwrap()
            .insert(id.to_string())
            .then(|| ActionGuard {
                store: self.clone(),
                id: id.to_string(),
            })
    }
}

/// CPU share of the whole machine, 0..100, from two cumulative readings.
pub fn cpu_pct(prev_usec: u64, cur_usec: u64, wall_secs: f64, ncpus: f64) -> f64 {
    if wall_secs <= 0.0 || ncpus <= 0.0 || cur_usec < prev_usec {
        return 0.0;
    }
    let pct = (cur_usec - prev_usec) as f64 / 1e6 / (wall_secs * ncpus) * 100.0;
    (pct.clamp(0.0, 100.0) * 10.0).round() / 10.0
}

#[derive(Debug, Clone, PartialEq)]
pub struct CgroupStats {
    pub cpu_usec: u64,
    pub mem_bytes: u64,
    /// 0 when unlimited.
    pub mem_limit_bytes: u64,
}

/// `/system.slice/minecraft@a.service/x/y` gives `/system.slice/minecraft@a.service`.
fn unit_cgroup(control_group: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in control_group.split('/').filter(|p| !p.is_empty()) {
        if part == ".." || part == "." {
            return None;
        }
        out.push(part);
        if part.ends_with(".service") {
            break;
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

pub fn read_cgroup(root: &Path, control_group: &str) -> Option<CgroupStats> {
    let dir = root.join(unit_cgroup(control_group)?);
    let num = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let mem_bytes = num("memory.current")?.trim().parse().ok()?;
    let mem_limit_bytes = num("memory.max")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let cpu_usec = num("cpu.stat")
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("usage_usec "))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    Some(CgroupStats {
        cpu_usec,
        mem_bytes,
        mem_limit_bytes,
    })
}

#[derive(Debug)]
pub struct Reading {
    /// systemd `ActiveState`, or `""` when the unit is not known.
    pub active_state: String,
    pub cgroup: Option<CgroupStats>,
}

/// Unit state and, when it is active, its cgroup counters. An instance the
/// backend does not know reads as an empty state, not an error.
pub async fn read_service(
    backend: &dyn ServiceBackend,
    instance: &str,
    cgroup_root: &Path,
) -> Result<Reading, AppError> {
    let unit = match backend.unit_status(instance).await {
        Ok(u) => u,
        Err(AppError::Io(e)) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let Some(unit) = unit else {
        return Ok(Reading {
            active_state: String::new(),
            cgroup: None,
        });
    };
    let cgroup = if unit.active_state == "active" {
        match backend.server_process(instance).await {
            Ok(Some(p)) => read_cgroup(cgroup_root, &p.control_group),
            _ => None,
        }
    } else {
        None
    };
    Ok(Reading {
        active_state: unit.active_state,
        cgroup,
    })
}

/// Cumulative CPU reading per service: (unix seconds, cpu usec).
pub type Prev = HashMap<String, (i64, u64)>;

/// One sampling round. A service that fails is skipped and logged; the others still get theirs.
pub async fn sample_all(
    backend: &dyn ServiceBackend,
    config: &Config,
    store: &Store,
    prev: &mut Prev,
    now: i64,
    cgroup_root: &Path,
    ncpus: f64,
) -> usize {
    let mut recorded = 0;
    for svc in &config.services {
        let Some(instance) = instance_of(svc) else {
            continue;
        };
        let reading = match read_service(backend, instance, cgroup_root).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("metrics: skipping {:?}: {e}", svc.id);
                continue;
            }
        };
        let up = reading.active_state == "active";
        let (mut cpu, mut mem) = (0.0, 0);
        match reading.cgroup {
            Some(c) if up => {
                mem = c.mem_bytes;
                if let Some((t0, u0)) = prev.get(&svc.id) {
                    cpu = cpu_pct(*u0, c.cpu_usec, (now - t0) as f64, ncpus);
                }
                prev.insert(svc.id.clone(), (now, c.cpu_usec));
            }
            _ => {
                prev.remove(&svc.id);
            }
        }
        store.record(
            &svc.id,
            Sample {
                t: bucket(now),
                cpu_pct: cpu,
                mem_bytes: mem,
                up,
            },
        );
        recorded += 1;
    }
    recorded
}

static STORE: OnceLock<Arc<Store>> = OnceLock::new();
static SAMPLER: OnceLock<()> = OnceLock::new();

/// The process-wide store, loaded from `MCSV_METRICS_PATH` (default `DEFAULT_PATH`) on first use.
pub fn store() -> Arc<Store> {
    STORE
        .get_or_init(|| {
            let path = std::env::var("MCSV_METRICS_PATH").unwrap_or_else(|_| DEFAULT_PATH.into());
            Arc::new(Store::load(Path::new(&path)))
        })
        .clone()
}

/// Returns the store and starts the 5-minute sampler the first time it is called.
/// (`AppState` and `main` are outside this task, so the first API request starts it.)
pub fn ensure_started(state: &Arc<AppState>) -> Arc<Store> {
    let store = store();
    if !cfg!(test) {
        SAMPLER.get_or_init(|| {
            let (backend, config, store) =
                (state.backend.clone(), state.config.clone(), store.clone());
            tokio::spawn(async move {
                let ncpus = std::thread::available_parallelism().map_or(1.0, |n| n.get() as f64);
                let mut prev = Prev::new();
                let mut tick = tokio::time::interval(Duration::from_secs(BUCKET_SECS as u64));
                loop {
                    tick.tick().await;
                    let now = chrono::Utc::now().timestamp();
                    sample_all(
                        backend.as_ref(),
                        &config,
                        &store,
                        &mut prev,
                        now,
                        Path::new(CGROUP_ROOT),
                        ncpus,
                    )
                    .await;
                    let s = store.clone();
                    match tokio::task::spawn_blocking(move || s.save()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("metrics: cannot save: {e}"),
                        Err(e) => eprintln!("metrics: save task failed: {e}"),
                    }
                }
            });
        });
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FakeBackend;

    fn sample(t: i64, up: bool) -> Sample {
        Sample {
            t,
            cpu_pct: 1.5,
            mem_bytes: 10,
            up,
        }
    }

    #[test]
    fn buckets_are_five_minutes() {
        assert_eq!(bucket(1_000_000_123), 1_000_000_123 - 1_000_000_123 % 300);
        assert_eq!(bucket(600), 600);
        assert_eq!(bucket(899), 600);
    }

    #[test]
    fn cpu_pct_is_a_share_of_the_whole_machine() {
        // 60 cpu-seconds over 300 s on 4 cores = 5 %.
        assert_eq!(cpu_pct(0, 60_000_000, 300.0, 4.0), 5.0);
        assert_eq!(cpu_pct(0, 900_000_000, 300.0, 1.0), 100.0);
        assert_eq!(cpu_pct(10, 5, 300.0, 4.0), 0.0, "counter went backwards");
        assert_eq!(cpu_pct(0, 5, 0.0, 4.0), 0.0);
    }

    #[test]
    fn metrics_window_is_oldest_first_and_omits_gaps() {
        let s = Store::in_memory();
        let now = 1_000_000_000 - 1_000_000_000 % 300;
        for t in [now - 600, now - 3600 * 30, now, now - 300 * 5] {
            s.record("a", sample(t, true));
        }
        let got: Vec<i64> = s.metrics("a", 24, now).iter().map(|s| s.t).collect();
        assert_eq!(got, vec![now - 1500, now - 600, now]);
        assert!(s.metrics("nope", 24, now).is_empty());
    }

    #[test]
    fn recording_a_bucket_again_replaces_it_and_old_samples_expire() {
        let s = Store::in_memory();
        s.record("a", sample(1_000, true));
        s.record(
            "a",
            Sample {
                cpu_pct: 9.0,
                ..sample(1_000, true)
            },
        );
        assert_eq!(s.metrics("a", 168, 1_000).len(), 1);
        assert_eq!(s.latest("a").unwrap().cpu_pct, 9.0);
        s.record("a", sample(1_000 + RETENTION_SECS + 300, true));
        assert_eq!(s.samples.lock().unwrap()["a"].len(), 1);
    }

    #[test]
    fn uptime_is_the_up_share_per_hour_and_skips_empty_hours() {
        let s = Store::in_memory();
        let hour = 3600 * 100_000;
        for i in 0..12 {
            s.record("a", sample(hour + i * 300, i < 9));
        }
        // Next hour has no samples; the one after has one up sample.
        s.record("a", sample(hour + 2 * 3600, true));
        let got = s.uptime("a", 7, hour + 2 * 3600 + 10);
        assert_eq!(
            got,
            vec![
                UptimePoint {
                    t: hour,
                    up_ratio: 0.75
                },
                UptimePoint {
                    t: hour + 7200,
                    up_ratio: 1.0
                },
            ]
        );
        // Outside the window.
        assert!(s.uptime("a", 1, hour + 10 * 24 * 3600).is_empty());
    }

    #[test]
    fn the_file_round_trips_atomically_and_a_corrupt_file_starts_empty() {
        let dir = std::env::temp_dir().join(format!("t26-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.json");
        let s = Store::load(&path);
        s.record("a", sample(300, true));
        s.save().unwrap();
        assert!(!dir.join("metrics.json.tmp").exists());
        assert_eq!(Store::load(&path).latest("a"), Some(sample(300, true)));
        std::fs::write(&path, "{not json").unwrap();
        assert!(Store::load(&path).latest("a").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn observe_tracks_since_and_last_restart() {
        let s = Store::in_memory();
        assert_eq!(s.observe("a", "up", 100, Some(50)), (50, None));
        assert_eq!(s.observe("a", "up", 200, None), (50, None));
        assert_eq!(s.observe("a", "down", 300, None), (300, None));
        assert_eq!(s.observe("a", "up", 400, None), (400, Some(400)));
        s.mark_restart("a", 500);
        assert!(s.restart_recent("a", 550));
        assert!(!s.restart_recent("a", 500 + RESTART_WINDOW_SECS));
        assert!(!s.restart_recent("other", 0));
    }

    #[test]
    fn only_one_action_per_service_runs_at_a_time() {
        let s = Arc::new(Store::in_memory());
        let g = s.begin_action("a").unwrap();
        assert!(s.begin_action("a").is_none());
        assert!(s.begin_action("b").is_some());
        drop(g);
        assert!(s.begin_action("a").is_some());
    }

    #[test]
    fn cgroup_files_are_read_from_the_unit_cgroup() {
        let root = std::env::temp_dir().join(format!("t26-cg-{}", std::process::id()));
        let unit = root.join("system.slice/minecraft@a.service");
        std::fs::create_dir_all(unit.join("sub")).unwrap();
        std::fs::write(unit.join("cpu.stat"), "usage_usec 4200\nuser_usec 1\n").unwrap();
        std::fs::write(unit.join("memory.current"), "2048\n").unwrap();
        std::fs::write(unit.join("memory.max"), "max\n").unwrap();
        let want = CgroupStats {
            cpu_usec: 4200,
            mem_bytes: 2048,
            mem_limit_bytes: 0,
        };
        assert_eq!(
            read_cgroup(&root, "/system.slice/minecraft@a.service/sub"),
            Some(want.clone())
        );
        std::fs::write(unit.join("memory.max"), "4096\n").unwrap();
        assert_eq!(
            read_cgroup(&root, "/system.slice/minecraft@a.service")
                .unwrap()
                .mem_limit_bytes,
            4096
        );
        assert_eq!(read_cgroup(&root, "/system.slice/../x.service"), None);
        assert_eq!(
            read_cgroup(&root, "/system.slice/minecraft@zz.service"),
            None
        );
        assert_eq!(read_cgroup(&root, ""), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn config(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    #[tokio::test]
    async fn a_failing_service_does_not_stop_the_others_being_sampled() {
        let cfg = config(
            "[[service]]\nid='ghost'\nname='G'\nbackend='systemd'\nunit='minecraft@ghost.service'\n\
             [[service]]\nid='survival'\nname='S'\nbackend='systemd'\nunit='minecraft@survival.service'\n\
             [[service]]\nid='other'\nname='O'\nbackend='systemd'\nunit='ssh.service'\n",
        );
        // `ghost` fails to read; `survival` must still be sampled.
        let backend = FakeBackend::new(&["survival", "ghost"]);
        backend.fail_status("ghost");
        let store = Store::in_memory();
        let mut prev = Prev::new();
        let n = sample_all(
            &backend,
            &cfg,
            &store,
            &mut prev,
            1_000,
            Path::new("/nonexistent"),
            1.0,
        )
        .await;
        assert_eq!(n, 1);
        assert!(store.latest("ghost").is_none());
        assert_eq!(
            store.latest("survival"),
            Some(Sample {
                t: 900,
                cpu_pct: 0.0,
                mem_bytes: 0,
                up: true
            })
        );
        assert!(
            store.latest("other").is_none(),
            "non-minecraft units are not sampled"
        );
    }
}
