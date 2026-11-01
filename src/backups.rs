//! Backup manifests and requests (D6, D6a). The manager never runs a dump: it
//! reads the manifests the root-owned `chulacraft-backup@<target>.service` writes,
//! and hands a request to that unit.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde_json::{Value, json};

use crate::{backend::ServiceBackend, handlers::AppError};

pub const TARGETS: [&str; 3] = ["world", "supabase", "bot"];
pub const DEFAULT_MANIFEST_DIR: &str = "/var/lib/chulacraft-backup/manifest";
pub const DEFAULT_REQUEST_DIR: &str = "/var/lib/mcsv_manager/backup-requests";

#[derive(Debug, Clone)]
pub struct Dirs {
    pub manifests: PathBuf,
    pub requests: PathBuf,
}

impl Dirs {
    /// `MCSV_BACKUP_MANIFEST_DIR` and `MCSV_BACKUP_REQUEST_DIR` exist for tests; the
    /// defaults are the contract's paths.
    pub fn from_env() -> Self {
        let dir = |var, default: &str| {
            std::env::var_os(var).map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        Self {
            manifests: dir("MCSV_BACKUP_MANIFEST_DIR", DEFAULT_MANIFEST_DIR),
            requests: dir("MCSV_BACKUP_REQUEST_DIR", DEFAULT_REQUEST_DIR),
        }
    }
}

pub fn unit_for(target: &str) -> String {
    format!("chulacraft-backup@{target}.service")
}

/// `^bk-[0-9TZ]+-(world|supabase|bot)$`, the same check the script makes.
pub fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("bk-") else {
        return false;
    };
    TARGETS.iter().any(|t| {
        rest.strip_suffix(t)
            .and_then(|r| r.strip_suffix('-'))
            .is_some_and(|stamp| {
                !stamp.is_empty()
                    && stamp
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"TZ".contains(&b))
            })
    })
}

fn target_of(id: &str) -> Option<&'static str> {
    TARGETS.into_iter().find(|t| id.ends_with(&format!("-{t}")))
}

/// `bk-20261022T090120Z-world` to `2026-10-22T09:01:20Z`.
fn created_at_of(id: &str) -> Option<String> {
    let stamp = id.strip_prefix("bk-")?.split('-').next()?;
    let t = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").ok()?;
    Some(t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn manifest_ok(m: &Value) -> bool {
    let s = |k: &str| m.get(k).and_then(Value::as_str);
    s("id").is_some_and(valid_id)
        && s("target").is_some_and(|t| TARGETS.contains(&t))
        && matches!(s("kind"), Some("manual" | "nightly"))
        && matches!(s("status"), Some("running" | "ok" | "failed"))
        && m.get("size_bytes").is_some_and(Value::is_u64)
        && s("created_at").is_some()
}

/// Manifests newest first, plus a `running` entry for any pending request whose
/// unit is active. A `running` manifest whose unit is no longer active (a killed
/// script) is reported as `failed`.
pub async fn list(dirs: &Dirs, backend: &dyn ServiceBackend) -> Result<Vec<Value>, AppError> {
    let mut out: Vec<Value> = vec![];
    match fs::read_dir(&dirs.manifests) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let parsed = fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok());
                if let Some(mut m) = parsed.filter(manifest_ok) {
                    if m["status"] == "running" {
                        let target = m["target"].as_str().unwrap_or_default().to_string();
                        if !backend.unit_active(&unit_for(&target)).await? {
                            m["status"] = json!("failed");
                        }
                    }
                    out.push(project(&m));
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for target in TARGETS {
        let Ok(id) = fs::read_to_string(dirs.requests.join(format!("{target}.id"))) else {
            continue;
        };
        let id = id.trim();
        if !valid_id(id)
            || target_of(id) != Some(target)
            || out.iter().any(|m| m["id"] == id)
            || !backend.unit_active(&unit_for(target)).await?
        {
            continue;
        }
        out.push(json!({
            "id": id, "target": target, "kind": "manual", "size_bytes": 0,
            "created_at": created_at_of(id).unwrap_or_default(), "status": "running",
        }));
    }
    out.sort_by(|a, b| {
        (b["created_at"].as_str(), b["id"].as_str())
            .cmp(&(a["created_at"].as_str(), a["id"].as_str()))
    });
    Ok(out)
}

fn project(m: &Value) -> Value {
    json!({
        "id": m["id"], "target": m["target"], "kind": m["kind"],
        "size_bytes": m["size_bytes"], "created_at": m["created_at"], "status": m["status"],
    })
}

pub enum RunError {
    /// 400: not one of the three targets.
    UnknownTarget,
    /// 409: that target is already running.
    Running,
    App(AppError),
}

/// Writes the request file and starts the unit. If starting fails, deletes the
/// file and passes the error on (the caller answers 500).
pub async fn request(
    dirs: &Dirs,
    backend: &dyn ServiceBackend,
    target: &str,
) -> Result<String, RunError> {
    if !TARGETS.contains(&target) {
        return Err(RunError::UnknownTarget);
    }
    let unit = unit_for(target);
    if backend.unit_active(&unit).await.map_err(RunError::App)? {
        return Err(RunError::Running);
    }
    let id = format!("bk-{}-{target}", Utc::now().format("%Y%m%dT%H%M%SZ"));
    let file = dirs.requests.join(format!("{target}.id"));
    write_request(&dirs.requests, &file, &id).map_err(|e| RunError::App(e.into()))?;
    if let Err(e) = backend.start_unit(&unit).await {
        let _ = fs::remove_file(&file);
        return Err(RunError::App(e));
    }
    Ok(id)
}

fn write_request(dir: &Path, file: &Path, id: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = file.with_extension("id.tmp");
    fs::write(&tmp, format!("{id}\n"))?;
    fs::rename(tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FakeBackend;

    fn dirs(name: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("mcsv-bk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("m")).unwrap();
        Dirs {
            manifests: root.join("m"),
            requests: root.join("r"),
        }
    }

    fn manifest(d: &Dirs, id: &str, created: &str, status: &str) {
        let target = target_of(id).unwrap();
        fs::write(
            d.manifests.join(format!("{id}.json")),
            json!({"id": id, "target": target, "kind": "nightly", "size_bytes": 5,
                   "created_at": created, "status": status, "extra": "dropped"})
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn id_validation_matches_the_script() {
        for ok in ["bk-20261022T090120Z-world", "bk-1TZ-bot", "bk-2-supabase"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in [
            "",
            "bk--world",
            "bk-20261022T09-other",
            "bk-2026-10-world",
            "bk-1-world\n",
            "x-1-world",
            "bk-1-world-world",
            "bk-../x-world",
            "bk-1Z-worldx",
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn lists_newest_first_skips_malformed_and_drops_extra_fields() {
        let d = dirs("list");
        let b = FakeBackend::new(&[]);
        manifest(
            &d,
            "bk-20261020T040000Z-world",
            "2026-10-20T04:00:00Z",
            "ok",
        );
        manifest(
            &d,
            "bk-20261022T040000Z-bot",
            "2026-10-22T04:00:00Z",
            "failed",
        );
        fs::write(d.manifests.join("junk.json"), "{not json").unwrap();
        fs::write(
            d.manifests.join("bad.json"),
            r#"{"id":"../../x","target":"world"}"#,
        )
        .unwrap();
        fs::write(d.manifests.join("note.txt"), "x").unwrap();
        let l = list(&d, &b).await.unwrap();
        let ids: Vec<_> = l.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["bk-20261022T040000Z-bot", "bk-20261020T040000Z-world"]
        );
        assert!(l[0].get("extra").is_none());
    }

    #[tokio::test]
    async fn a_missing_manifest_dir_is_an_empty_list() {
        let d = Dirs {
            manifests: "/nonexistent/mcsv".into(),
            requests: "/nonexistent/r".into(),
        };
        assert!(list(&d, &FakeBackend::new(&[])).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn running_manifest_without_an_active_unit_is_failed() {
        let d = dirs("stale");
        let b = FakeBackend::new(&[]);
        manifest(
            &d,
            "bk-20261022T040000Z-world",
            "2026-10-22T04:00:00Z",
            "running",
        );
        assert_eq!(list(&d, &b).await.unwrap()[0]["status"], "failed");
        b.start_unit("chulacraft-backup@world.service")
            .await
            .unwrap();
        assert_eq!(list(&d, &b).await.unwrap()[0]["status"], "running");
    }

    #[tokio::test]
    async fn a_pending_request_with_an_active_unit_lists_as_running() {
        let d = dirs("pending");
        let b = FakeBackend::new(&[]);
        fs::create_dir_all(&d.requests).unwrap();
        fs::write(d.requests.join("bot.id"), "bk-20261022T040000Z-bot\n").unwrap();
        // Wrong target in the file, and bad ids, never show.
        fs::write(d.requests.join("world.id"), "bk-20261022T040000Z-bot\n").unwrap();
        fs::write(d.requests.join("supabase.id"), "../etc\n").unwrap();
        assert!(list(&d, &b).await.unwrap().is_empty(), "unit not active");
        for t in TARGETS {
            b.start_unit(&unit_for(t)).await.unwrap();
        }
        let l = list(&d, &b).await.unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["status"], "running");
        assert_eq!(l[0]["kind"], "manual");
        assert_eq!(l[0]["created_at"], "2026-10-22T04:00:00Z");
    }

    #[tokio::test]
    async fn request_writes_the_id_file_and_starts_the_unit() {
        let d = dirs("req");
        let b = FakeBackend::new(&[]);
        let id = request(&d, &b, "world").await.ok().unwrap();
        assert!(valid_id(&id) && id.ends_with("-world"), "{id}");
        assert_eq!(
            fs::read_to_string(d.requests.join("world.id")).unwrap(),
            format!("{id}\n")
        );
        assert_eq!(
            *b.calls.lock().unwrap(),
            vec!["unit chulacraft-backup@world.service"]
        );
        assert!(matches!(
            request(&d, &b, "world").await,
            Err(RunError::Running)
        ));
        assert!(matches!(
            request(&d, &b, "etc").await,
            Err(RunError::UnknownTarget)
        ));
        assert!(matches!(
            request(&d, &b, "../world").await,
            Err(RunError::UnknownTarget)
        ));
    }
}
