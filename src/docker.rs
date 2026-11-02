//! `DockerClient` over the socket proxy (T2.7), and the filtering proxy's policy
//! (`proxy`). The manager never talks to `/var/run/docker.sock`: only to
//! `mcsv-docker-proxy`, which allows the calls in docs/ops-manager-contract.md
//! ("Docker socket proxy"), narrowed to the containers in the manager config.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use crate::config::{Action, Config, ServiceConfig};

pub const DEFAULT_PROXY_SOCKET: &str = "/run/mcsv-docker-proxy/docker.sock";
pub const DEFAULT_UPSTREAM_SOCKET: &str = "/var/run/docker.sock";
/// The registry digest is looked up at most this often per image.
pub const IMAGE_CACHE_TTL: Duration = Duration::from_secs(3600);
const MAX_RESPONSE: u64 = 32 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Minimal HTTP/1.0 client over a unix socket. HTTP/1.0 keeps Docker from
// streaming a response we would have to frame; the body ends at EOF.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub async fn http_request(socket: &Path, method: &str, target: &str) -> io::Result<HttpResponse> {
    let fut = async {
        let mut stream = UnixStream::connect(socket).await?;
        let req =
            format!("{method} {target} HTTP/1.0\r\nHost: docker\r\nContent-Length: 0\r\n\r\n");
        stream.write_all(req.as_bytes()).await?;
        let mut raw = Vec::new();
        (&mut stream)
            .take(MAX_RESPONSE)
            .read_to_end(&mut raw)
            .await?;
        parse_response(&raw)
    };
    tokio::time::timeout(IO_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "docker request timed out"))?
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn parse_response(raw: &[u8]) -> io::Result<HttpResponse> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("truncated HTTP response"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad("non-utf8 HTTP head"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad("bad HTTP status line"))?;
    let mut content_type = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-type" {
                content_type = Some(v.to_string());
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let body = &raw[split + 4..];
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

fn dechunk(mut b: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| bad("bad chunk"))?;
        let size = std::str::from_utf8(&b[..eol])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok())
            .ok_or_else(|| bad("bad chunk size"))?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err(bad("short chunk"));
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    /// The proxy (or Docker behind it) cannot be reached: 503.
    #[error("docker unavailable: {0}")]
    Unavailable(String),
    /// 404.
    #[error("not found")]
    NotFound,
    /// The proxy said 403: the container or call is not allowed.
    #[error("denied by docker proxy")]
    Denied,
    #[error("docker error: {0}")]
    Other(String),
}

impl DockerError {
    /// The HTTP status the manager API answers with.
    pub fn status(&self) -> u16 {
        match self {
            DockerError::Unavailable(_) => 503,
            DockerError::NotFound => 404,
            DockerError::Denied | DockerError::Other(_) => 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DockerStatus {
    pub state: &'static str,
    pub since: Option<String>,
    pub last_restart: Option<String>,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub mem_limit_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DockerLogLine {
    pub message: String,
    /// Unix milliseconds.
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImageInfo {
    pub current: String,
    pub latest: String,
    pub checked_at: String,
}

type CachedDigest = (String, DateTime<Utc>, Instant);

#[derive(Debug)]
pub struct DockerClient {
    socket: PathBuf,
    /// image reference -> (latest short digest, when the registry lookup ran)
    cache: Mutex<HashMap<String, CachedDigest>>,
}

/// The process-wide client. The socket comes from `MCSV_DOCKER_SOCKET` (default
/// `DEFAULT_PROXY_SOCKET`); one client keeps the hourly registry cache.
pub fn client() -> &'static DockerClient {
    static CLIENT: OnceLock<DockerClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        DockerClient::new(
            std::env::var("MCSV_DOCKER_SOCKET").unwrap_or_else(|_| DEFAULT_PROXY_SOCKET.into()),
        )
    })
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let t = DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc);
    // Docker's zero time, for a container that never started or stopped.
    (t.timestamp() > 0).then_some(t)
}

/// `name` -> `name:latest` when it carries neither a tag nor a digest.
pub fn with_tag(image: &str) -> String {
    let last = image.rsplit('/').next().unwrap_or(image);
    if last.contains(':') || last.contains('@') {
        image.to_string()
    } else {
        format!("{image}:latest")
    }
}

fn repo_of(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    match image.rfind(':') {
        Some(i) if !image[i..].contains('/') => &image[..i],
        _ => image,
    }
}

fn short_digest(d: &str) -> Option<String> {
    let hex = d.strip_prefix("sha256:")?;
    (hex.len() >= 8 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex[..8].to_string())
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(v, |v, k| v.get(k))
        .and_then(Value::as_str)
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter()
        .try_fold(v, |v, k| v.get(k))
        .and_then(Value::as_u64)
}

impl DockerClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn call(&self, method: &str, target: &str) -> Result<HttpResponse, DockerError> {
        let res = http_request(&self.socket, method, target)
            .await
            .map_err(|e| match e.kind() {
                io::ErrorKind::InvalidData => DockerError::Other(e.to_string()),
                _ => DockerError::Unavailable(e.to_string()),
            })?;
        match res.status {
            200..=299 => Ok(res),
            403 => Err(DockerError::Denied),
            404 => Err(DockerError::NotFound),
            502..=504 => Err(DockerError::Unavailable(format!("status {}", res.status))),
            s => Err(DockerError::Other(format!("status {s}"))),
        }
    }

    async fn json(&self, target: &str) -> Result<Value, DockerError> {
        let res = self.call("GET", target).await?;
        serde_json::from_slice(&res.body).map_err(|e| DockerError::Other(e.to_string()))
    }

    pub async fn inspect(&self, container: &str) -> Result<Value, DockerError> {
        self.json(&format!("/containers/{container}/json")).await
    }

    /// Raw `stats?stream=false`.
    pub async fn stats(&self, container: &str) -> Result<Value, DockerError> {
        self.json(&format!("/containers/{container}/stats?stream=false"))
            .await
    }

    pub async fn status(&self, container: &str) -> Result<DockerStatus, DockerError> {
        let info = self.inspect(container).await?;
        let mut status = status_from_inspect(&info);
        if status.state == "up" {
            let stats = self.stats(container).await?;
            let (cpu, mem) = usage_from_stats(&stats);
            status.cpu_pct = cpu;
            status.mem_bytes = mem;
        }
        Ok(status)
    }

    /// One metrics sample: (cpu_pct, mem_bytes).
    pub async fn sample(&self, container: &str) -> Result<(f64, u64), DockerError> {
        Ok(usage_from_stats(&self.stats(container).await?))
    }

    /// Newest first, like `GET /service/:id/rlog`.
    pub async fn logs(
        &self,
        container: &str,
        until: Option<u64>,
        max_lines: u64,
    ) -> Result<Vec<DockerLogLine>, DockerError> {
        let mut q = format!(
            "stdout=1&stderr=1&timestamps=1&tail={}",
            max_lines.clamp(1, 10_000)
        );
        if let Some(u) = until {
            q.push_str(&format!("&until={}", u + 1));
        }
        let res = self
            .call("GET", &format!("/containers/{container}/logs?{q}"))
            .await?;
        let mut lines = parse_logs(&res.body);
        lines.reverse();
        Ok(lines)
    }

    pub async fn act(&self, container: &str, action: Action) -> Result<(), DockerError> {
        let verb = match action {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
        };
        match self
            .call("POST", &format!("/containers/{container}/{verb}"))
            .await
        {
            Ok(_) => Ok(()),
            // 304 is "already in that state", which Docker answers as an error status.
            Err(DockerError::Other(m)) if m == "status 304" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Forget the cached registry digests, for example right after an update.
    pub fn forget_images(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// `GET /service/:id/image`. Never pulls; the registry lookup is cached for
    /// `IMAGE_CACHE_TTL` and an unreachable registry serves the cached value.
    pub async fn image(&self, container: &str) -> Result<ImageInfo, DockerError> {
        let info = self.inspect(container).await?;
        let reference = with_tag(
            str_at(&info, &["Config", "Image"])
                .ok_or_else(|| DockerError::Other("container has no image".into()))?,
        );
        let image_id = str_at(&info, &["Image"]).unwrap_or(&reference).to_string();
        let image = self.json(&format!("/images/{image_id}/json")).await?;
        let repo = repo_of(&reference);
        let current_digest = image
            .get("RepoDigests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|d| d.split_once('@'))
            .find(|(r, _)| repo_of(r) == repo)
            .and_then(|(_, d)| short_digest(d));
        let Some(current_digest) = current_digest else {
            let same = format!("{reference}@unknown");
            return Ok(ImageInfo {
                current: same.clone(),
                latest: same,
                checked_at: rfc3339(Utc::now()),
            });
        };
        let current = format!("{reference}@{current_digest}");
        let cached = self.cache.lock().unwrap().get(&reference).cloned();
        if let Some((latest, at, when)) = &cached
            && when.elapsed() < IMAGE_CACHE_TTL
        {
            return Ok(ImageInfo {
                current,
                latest: format!("{reference}@{latest}"),
                checked_at: rfc3339(*at),
            });
        }
        let looked_up = self
            .json(&format!("/distribution/{reference}/json"))
            .await
            .ok()
            .and_then(|v| str_at(&v, &["Descriptor", "digest"]).and_then(short_digest));
        match (looked_up, cached) {
            (Some(latest), _) => {
                let now = Utc::now();
                self.cache
                    .lock()
                    .unwrap()
                    .insert(reference.clone(), (latest.clone(), now, Instant::now()));
                Ok(ImageInfo {
                    current,
                    latest: format!("{reference}@{latest}"),
                    checked_at: rfc3339(now),
                })
            }
            // Registry unreachable: the stale value, or "up to date".
            (None, Some((latest, at, _))) => Ok(ImageInfo {
                current,
                latest: format!("{reference}@{latest}"),
                checked_at: rfc3339(at),
            }),
            (None, None) => Ok(ImageInfo {
                latest: current.clone(),
                current,
                checked_at: rfc3339(Utc::now()),
            }),
        }
    }
}

pub fn status_from_inspect(info: &Value) -> DockerStatus {
    let state = match str_at(info, &["State", "Status"]) {
        Some("running") => "up",
        Some("restarting") => "restarting",
        Some("exited" | "created" | "dead" | "paused" | "removing") => "down",
        _ => "unknown",
    };
    let started = str_at(info, &["State", "StartedAt"]).and_then(parse_time);
    let finished = str_at(info, &["State", "FinishedAt"]).and_then(parse_time);
    let since = match state {
        "up" => started,
        _ => finished.or(started),
    }
    .or_else(|| str_at(info, &["Created"]).and_then(parse_time));
    let restarted = u64_at(info, &["RestartCount"]).unwrap_or(0) > 0;
    DockerStatus {
        state,
        since: since.map(rfc3339),
        last_restart: if restarted {
            started.map(rfc3339)
        } else {
            None
        },
        cpu_pct: 0.0,
        mem_bytes: 0,
        mem_limit_bytes: u64_at(info, &["HostConfig", "Memory"]).unwrap_or(0),
    }
}

/// (cpu_pct of the whole host capacity 0..100, memory without page cache).
pub fn usage_from_stats(stats: &Value) -> (f64, u64) {
    let d = |path_now: &[&str], path_prev: &[&str]| {
        u64_at(stats, path_now)
            .unwrap_or(0)
            .saturating_sub(u64_at(stats, path_prev).unwrap_or(0)) as f64
    };
    let cpu = d(
        &["cpu_stats", "cpu_usage", "total_usage"],
        &["precpu_stats", "cpu_usage", "total_usage"],
    );
    let sys = d(
        &["cpu_stats", "system_cpu_usage"],
        &["precpu_stats", "system_cpu_usage"],
    );
    let pct = if sys > 0.0 {
        (cpu / sys * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let pct = (pct * 10.0).round() / 10.0;
    let usage = u64_at(stats, &["memory_stats", "usage"]).unwrap_or(0);
    let cache = u64_at(stats, &["memory_stats", "stats", "inactive_file"])
        .or_else(|| u64_at(stats, &["memory_stats", "stats", "cache"]))
        .unwrap_or(0);
    (pct, usage.saturating_sub(cache))
}

/// Docker's multiplexed log stream (8-byte frame headers), or plain text for a
/// container with a TTY; with `timestamps=1` each line starts with an RFC 3339 time.
pub fn parse_logs(body: &[u8]) -> Vec<DockerLogLine> {
    let mut text = Vec::new();
    let framed = body.first().is_some_and(|b| *b <= 2) && body.len() >= 8;
    if framed {
        let mut b = body;
        while b.len() >= 8 {
            let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize;
            let end = (8 + len).min(b.len());
            text.extend_from_slice(&b[8..end]);
            b = &b[end..];
        }
    } else {
        text.extend_from_slice(body);
    }
    String::from_utf8_lossy(&text)
        .lines()
        .map(|l| {
            let (ts, msg) = l.split_once(' ').unwrap_or(("", l));
            match DateTime::parse_from_rfc3339(ts) {
                Ok(t) => DockerLogLine {
                    message: msg.to_string(),
                    timestamp: t.timestamp_millis().max(0) as u64,
                },
                Err(_) => DockerLogLine {
                    message: l.to_string(),
                    timestamp: 0,
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Update env files (generated at deploy time by `mcsv-docker-proxy update-env`)
// ---------------------------------------------------------------------------

/// The `EnvironmentFile` of `chulacraft-update@<id>.service`, or `None` when the
/// service is not updatable or a value could not be quoted safely.
pub fn update_env(s: &ServiceConfig) -> Result<Option<String>, io::Error> {
    let (Some(file), Some(service)) = (&s.compose_file, &s.compose_service) else {
        return Ok(None);
    };
    for v in [file, service] {
        if v.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "service {:?}: unsupported character in compose settings",
                    s.id
                ),
            ));
        }
    }
    Ok(Some(format!(
        "COMPOSE_FILE=\"{file}\"\nCOMPOSE_SERVICE=\"{service}\"\n"
    )))
}

/// Writes `<dir>/<id>.env` (0600) for every updatable service and removes other
/// `*.env` files, so a service dropped from the config can no longer be updated.
pub fn write_update_envs(config: &Config, dir: &Path) -> io::Result<Vec<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    for s in &config.services {
        if let Some(text) = update_env(s)? {
            let path = dir.join(format!("{}.env", s.id));
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            io::Write::write_all(&mut f, text.as_bytes())?;
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            written.push(format!("{}.env", s.id));
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".env") && !written.contains(&name) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Proxy policy
// ---------------------------------------------------------------------------

pub mod proxy {
    use std::future::Future;

    use super::*;

    /// The account the manager runs as; the only non-root peer the proxy answers.
    pub const MANAGER_USER: &str = "mcsv-mgr";

    /// Peers (by kernel-reported uid, SO_PEERCRED) the proxy answers: root and `allowed`.
    /// An unknown peer is refused.
    pub fn peer_allowed(uid: Option<u32>, allowed: &[u32]) -> bool {
        uid.is_some_and(|u| u == 0 || allowed.contains(&u))
    }

    /// `MCSV_DOCKER_PROXY_UIDS` (numeric uids, comma or space separated) if set, else the
    /// uid of `MANAGER_USER`. A bad entry or a missing user is an error: the proxy then
    /// does not start, rather than start open.
    pub fn allowed_peer_uids(raw: Option<&str>) -> Result<Vec<u32>, String> {
        if let Some(raw) = raw {
            return raw
                .split([',', ' ', '\n'])
                .filter(|p| !p.is_empty())
                .map(|p| {
                    p.parse::<u32>()
                        .map_err(|_| format!("MCSV_DOCKER_PROXY_UIDS: {p:?} is not a numeric uid"))
                })
                .collect();
        }
        let name = std::ffi::CString::new(MANAGER_USER).map_err(|e| e.to_string())?;
        // Called once at startup, before any threads need getpwnam.
        let user = unsafe { libc::getpwnam(name.as_ptr()) };
        if user.is_null() {
            return Err(format!("user {MANAGER_USER} not found"));
        }
        Ok(vec![unsafe { (*user).pw_uid }])
    }

    /// A request that passed the syntax check. Authorization (is the container
    /// configured?) is a second step, `serve`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Route {
        List { all: bool },
        Inspect(String),
        Logs(String, String),
        Stats(String),
        Action(String, &'static str, String),
        ImageInspect(String),
        Distribution(String),
    }

    /// Strips an optional `/v1.43` API version prefix.
    fn strip_version(path: &str) -> &str {
        if let Some(rest) = path.strip_prefix("/v") {
            let (ver, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            if !ver.is_empty() && ver.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return tail;
            }
        }
        path
    }

    fn token(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    }

    fn image_name(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 300
            && !s.starts_with('/')
            && !s.contains("..")
            && !s.contains("//")
            && s.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':' | b'/' | b'@')
            })
    }

    fn bool_val(v: &str) -> bool {
        matches!(v, "0" | "1" | "true" | "false")
    }

    type Check = fn(&str) -> bool;

    /// Validates the query against a per-route allowlist and returns it rebuilt.
    fn query(raw: Option<&str>, allowed: &[(&str, Check)]) -> Option<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        for pair in raw.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=')?;
            let ok = allowed.iter().find(|(name, _)| *name == k)?.1;
            if !ok(v) || out.iter().any(|(ok, _)| ok == k) {
                return None;
            }
            out.push((k.to_string(), v.to_string()));
        }
        Some(out)
    }

    fn join(q: &[(String, String)]) -> String {
        q.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    fn digits(v: &str) -> bool {
        !v.is_empty() && v.len() <= 12 && v.bytes().all(|b| b.is_ascii_digit())
    }

    fn tail(v: &str) -> bool {
        v == "all" || digits(v)
    }

    /// The syntax half of the allowlist. `None` means 403.
    pub fn classify(method: &str, target: &str) -> Option<Route> {
        let (path, raw_query) = match target.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (target, None),
        };
        // No encoding tricks: every allowed value is plain ASCII.
        if !path.is_ascii() || path.contains('%') || path.contains("//") {
            return None;
        }
        let path = strip_version(path);
        match method {
            "GET" => {
                if path == "/containers/json" {
                    let q = query(raw_query, &[("all", bool_val)])?;
                    return Some(Route::List {
                        all: q.iter().any(|(_, v)| v == "1" || v == "true"),
                    });
                }
                if let Some(rest) = path.strip_prefix("/containers/") {
                    let (id, tail_path) = rest.split_once('/')?;
                    if !token(id) {
                        return None;
                    }
                    return match tail_path {
                        "json" => {
                            query(raw_query, &[])?;
                            Some(Route::Inspect(id.into()))
                        }
                        "logs" => {
                            let q = query(
                                raw_query,
                                &[
                                    ("stdout", bool_val),
                                    ("stderr", bool_val),
                                    ("timestamps", bool_val),
                                    ("tail", tail),
                                    ("since", digits),
                                    ("until", digits),
                                ],
                            )?;
                            Some(Route::Logs(id.into(), join(&q)))
                        }
                        "stats" => {
                            let q = query(raw_query, &[("stream", bool_val)])?;
                            // Streaming stats never end: stream=false is required.
                            (q == [("stream".to_string(), "false".to_string())]
                                || q == [("stream".to_string(), "0".to_string())])
                            .then(|| Route::Stats(id.into()))
                        }
                        _ => None,
                    };
                }
                if let Some(rest) = path.strip_prefix("/images/") {
                    let name = rest.strip_suffix("/json")?;
                    query(raw_query, &[])?;
                    return image_name(name).then(|| Route::ImageInspect(name.into()));
                }
                if let Some(rest) = path.strip_prefix("/distribution/") {
                    let name = rest.strip_suffix("/json")?;
                    query(raw_query, &[])?;
                    return image_name(name).then(|| Route::Distribution(name.into()));
                }
                None
            }
            "POST" => {
                let rest = path.strip_prefix("/containers/")?;
                let (id, verb) = rest.split_once('/')?;
                let verb = ["start", "stop", "restart"]
                    .into_iter()
                    .find(|v| *v == verb)?;
                if !token(id) {
                    return None;
                }
                let q = query(raw_query, &[("t", digits)])?;
                Some(Route::Action(id.into(), verb, join(&q)))
            }
            _ => None,
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Reply {
        pub status: u16,
        pub content_type: String,
        pub body: Vec<u8>,
    }

    impl Reply {
        fn json(status: u16, v: Value) -> Self {
            Self {
                status,
                content_type: "application/json".into(),
                body: v.to_string().into_bytes(),
            }
        }

        fn forbidden() -> Self {
            Self::json(403, json!({ "message": "forbidden by mcsv-docker-proxy" }))
        }

        fn unavailable() -> Self {
            Self::json(502, json!({ "message": "docker unavailable" }))
        }

        fn from(res: HttpResponse) -> Self {
            Self {
                status: res.status,
                content_type: res
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".into()),
                body: res.body,
            }
        }
    }

    /// Where the proxy sends allowed requests: Docker's real socket in production.
    pub trait Upstream: Send + Sync {
        fn get(&self, target: &str) -> impl Future<Output = io::Result<HttpResponse>> + Send;
        fn post(&self, target: &str) -> impl Future<Output = io::Result<HttpResponse>> + Send;
    }

    pub struct SocketUpstream(pub PathBuf);

    impl Upstream for SocketUpstream {
        async fn get(&self, target: &str) -> io::Result<HttpResponse> {
            http_request(&self.0, "GET", target).await
        }
        async fn post(&self, target: &str) -> io::Result<HttpResponse> {
            http_request(&self.0, "POST", target).await
        }
    }

    /// The container names (without the leading `/`) from the manager config.
    pub fn containers(config: &Config) -> Vec<String> {
        config
            .services
            .iter()
            .filter_map(|s| s.container.clone())
            .collect()
    }

    /// Removes `Config.Env` (and the image-level copies): it holds the bot's tokens.
    pub fn strip_env(v: &mut Value) {
        for key in ["Config", "ContainerConfig"] {
            if let Some(obj) = v.get_mut(key).and_then(Value::as_object_mut) {
                obj.remove("Env");
            }
        }
    }

    fn name_of(v: &Value) -> Option<&str> {
        v.get("Name")?.as_str().map(|n| n.trim_start_matches('/'))
    }

    /// Resolves a name or id to (full id, name, inspect body) when it is configured.
    async fn resolve<U: Upstream>(
        allowed: &[String],
        up: &U,
        id: &str,
    ) -> Result<(String, Value), Reply> {
        let res = up
            .get(&format!("/containers/{id}/json"))
            .await
            .map_err(|_| Reply::unavailable())?;
        if res.status != 200 {
            // A configured container that does not exist is a plain 404; anything
            // else is indistinguishable from an unconfigured one.
            return Err(if res.status == 404 && allowed.iter().any(|a| a == id) {
                Reply::from(res)
            } else if res.status >= 500 {
                Reply::unavailable()
            } else {
                Reply::forbidden()
            });
        }
        let body: Value = serde_json::from_slice(&res.body).map_err(|_| Reply::unavailable())?;
        let name = name_of(&body).unwrap_or("");
        if !allowed.iter().any(|a| a == name) {
            return Err(Reply::forbidden());
        }
        let full = body
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(Reply::unavailable)?
            .to_string();
        Ok((full, body))
    }

    /// Image references the proxy answers for: those of configured containers.
    async fn allowed_images<U: Upstream>(allowed: &[String], up: &U) -> Vec<String> {
        let mut out = Vec::new();
        for name in allowed {
            let Ok(res) = up.get(&format!("/containers/{name}/json")).await else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<Value>(&res.body) else {
                continue;
            };
            if res.status != 200 || name_of(&v) != Some(name.as_str()) {
                continue;
            }
            if let Some(image) = str_at(&v, &["Config", "Image"]) {
                out.push(image.to_string());
                out.push(with_tag(image));
            }
            if let Some(id) = str_at(&v, &["Image"]) {
                out.push(id.to_string());
            }
        }
        out
    }

    pub async fn serve<U: Upstream>(
        allowed: &[String],
        up: &U,
        method: &str,
        target: &str,
    ) -> Reply {
        let Some(route) = classify(method, target) else {
            return Reply::forbidden();
        };
        match route {
            Route::List { all } => {
                let res = match up
                    .get(&format!("/containers/json?all={}", if all { 1 } else { 0 }))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => return Reply::unavailable(),
                };
                if res.status != 200 {
                    return Reply::from(res);
                }
                let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&res.body) else {
                    return Reply::unavailable();
                };
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|c| {
                        c.get("Names")
                            .and_then(Value::as_array)
                            .is_some_and(|names| {
                                names
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .any(|n| allowed.iter().any(|a| a == n.trim_start_matches('/')))
                            })
                    })
                    .collect();
                Reply::json(200, Value::Array(kept))
            }
            Route::Inspect(id) => match resolve(allowed, up, &id).await {
                Ok((_, mut body)) => {
                    strip_env(&mut body);
                    Reply::json(200, body)
                }
                Err(r) => r,
            },
            Route::Logs(id, q) => forward(allowed, up, &id, "logs", &q, false).await,
            Route::Stats(id) => forward(allowed, up, &id, "stats", "stream=false", false).await,
            Route::Action(id, verb, q) => forward(allowed, up, &id, verb, &q, true).await,
            Route::ImageInspect(name) => {
                if !allowed_images(allowed, up).await.contains(&name) {
                    return Reply::forbidden();
                }
                match up.get(&format!("/images/{name}/json")).await {
                    Ok(res) if res.status == 200 => {
                        match serde_json::from_slice::<Value>(&res.body) {
                            Ok(mut v) => {
                                strip_env(&mut v);
                                Reply::json(200, v)
                            }
                            Err(_) => Reply::unavailable(),
                        }
                    }
                    Ok(res) => Reply::from(res),
                    Err(_) => Reply::unavailable(),
                }
            }
            Route::Distribution(name) => {
                if !allowed_images(allowed, up).await.contains(&name) {
                    return Reply::forbidden();
                }
                match up.get(&format!("/distribution/{name}/json")).await {
                    Ok(res) => Reply::from(res),
                    Err(_) => Reply::unavailable(),
                }
            }
        }
    }

    async fn forward<U: Upstream>(
        allowed: &[String],
        up: &U,
        id: &str,
        verb: &str,
        query: &str,
        post: bool,
    ) -> Reply {
        let (full, _) = match resolve(allowed, up, id).await {
            Ok(r) => r,
            Err(r) => return r,
        };
        // The resolved full id, never the caller's string.
        let target = if query.is_empty() {
            format!("/containers/{full}/{verb}")
        } else {
            format!("/containers/{full}/{verb}?{query}")
        };
        let res = if post {
            up.post(&target).await
        } else {
            up.get(&target).await
        };
        match res {
            Ok(res) => Reply::from(res),
            Err(_) => Reply::unavailable(),
        }
    }
}

#[cfg(test)]
pub mod test_support {
    use std::path::{Path, PathBuf};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    /// Serves `routes` (path-with-query -> (status, content type, body)) on a unix socket.
    pub async fn fake_socket(
        dir: &Path,
        routes: Vec<(&'static str, u16, &'static str, Vec<u8>)>,
    ) -> PathBuf {
        let path = dir.join("docker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                let target = line.split(' ').nth(1).unwrap_or("").to_string();
                let found = routes.iter().find(|r| r.0 == target);
                let (status, ct, body) = match found {
                    Some((_, st, ct, b)) => (*st, *ct, b.clone()),
                    None => (404, "application/json", b"{}".to_vec()),
                };
                let mut out =
                    format!("HTTP/1.0 {status} X\r\nContent-Type: {ct}\r\n\r\n").into_bytes();
                out.extend(body);
                let _ = s.write_all(&out).await;
            }
        });
        path
    }

    pub fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("t27-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap as Map, sync::Arc};

    use super::proxy::*;
    use super::test_support::{fake_socket, tmpdir};
    use super::*;

    #[test]
    fn only_root_and_the_listed_uids_may_use_the_proxy() {
        let allowed = [995];
        assert!(peer_allowed(Some(0), &allowed));
        assert!(peer_allowed(Some(995), &allowed));
        assert!(!peer_allowed(Some(996), &allowed), "e.g. the game user");
        assert!(!peer_allowed(None, &allowed), "unknown peer");
        assert!(!peer_allowed(Some(996), &[]));
        assert_eq!(allowed_peer_uids(Some("12, 34")).unwrap(), vec![12, 34]);
        assert!(allowed_peer_uids(Some("mcsv")).is_err());
    }

    // ---- allow / deny table -------------------------------------------------

    #[test]
    fn every_allowed_and_denied_method_and_path() {
        let allowed = [
            ("GET", "/containers/json"),
            ("GET", "/containers/json?all=1"),
            ("GET", "/v1.43/containers/json"),
            ("GET", "/containers/bot/json"),
            ("GET", "/containers/0123abcd/json"),
            (
                "GET",
                "/containers/bot/logs?stdout=1&stderr=1&tail=200&until=1700000000&timestamps=1",
            ),
            ("GET", "/containers/bot/logs"),
            ("GET", "/containers/bot/stats?stream=false"),
            ("GET", "/containers/bot/stats?stream=0"),
            ("POST", "/containers/bot/start"),
            ("POST", "/containers/bot/stop?t=10"),
            ("POST", "/containers/bot/restart"),
            ("GET", "/images/ghcr.io/chulacraft/bot:1.4.2/json"),
            ("GET", "/images/sha256:0123abcd/json"),
            ("GET", "/distribution/ghcr.io/chulacraft/bot:1.4.2/json"),
        ];
        for (m, t) in allowed {
            assert!(
                classify(m, t).is_some(),
                "{m} {t} should pass the syntax check"
            );
        }
        let denied = [
            // other verbs on allowed paths
            ("POST", "/containers/json"),
            ("DELETE", "/containers/bot"),
            ("DELETE", "/containers/bot/json"),
            ("PUT", "/containers/bot/start"),
            ("GET", "/containers/bot/start"),
            ("POST", "/containers/bot/json"),
            ("HEAD", "/containers/bot/json"),
            ("POST", "/images/x/json"),
            // everything else the contract names
            ("POST", "/containers/create"),
            ("POST", "/containers/create?name=x"),
            ("POST", "/containers/bot/exec"),
            ("POST", "/containers/bot/kill"),
            ("POST", "/containers/bot/pause"),
            ("POST", "/containers/bot/update"),
            ("POST", "/containers/bot/rename?name=x"),
            ("POST", "/containers/bot/attach"),
            ("POST", "/containers/prune"),
            ("GET", "/containers/bot/archive?path=/etc"),
            ("GET", "/containers/bot/export"),
            ("GET", "/containers/bot/top"),
            ("GET", "/containers/bot/changes"),
            ("GET", "/containers/bot/attach/ws"),
            ("POST", "/exec/abc/start"),
            ("POST", "/build"),
            ("POST", "/images/create?fromImage=alpine"),
            ("POST", "/images/load"),
            ("GET", "/images/json"),
            ("GET", "/images/get?names=x"),
            ("DELETE", "/images/x"),
            ("GET", "/volumes"),
            ("POST", "/volumes/create"),
            ("GET", "/networks"),
            ("POST", "/networks/create"),
            ("GET", "/info"),
            ("GET", "/version"),
            ("GET", "/events"),
            ("GET", "/_ping"),
            ("GET", "/secrets"),
            ("GET", "/"),
            ("GET", ""),
            // query abuse
            ("GET", "/containers/bot/logs?follow=1"),
            ("GET", "/containers/bot/logs?tail=1&tail=2"),
            ("GET", "/containers/bot/logs?tail=-1"),
            ("GET", "/containers/bot/stats"),
            ("GET", "/containers/bot/stats?stream=true"),
            ("GET", "/containers/bot/stats?stream=false&x=1"),
            ("GET", "/containers/json?filters=%7B%7D"),
            ("GET", "/containers/json?size=1"),
            ("GET", "/containers/bot/json?size=1"),
            ("POST", "/containers/bot/stop?signal=KILL"),
            ("POST", "/containers/bot/start?detachKeys=x"),
            // path abuse
            ("GET", "/containers/../etc/json"),
            ("GET", "/containers/bot%2Fx/json"),
            ("GET", "/containers//json"),
            ("GET", "//containers/bot/json"),
            ("GET", "/containers/bot/json/extra"),
            ("GET", "/containers/a b/json"),
            ("GET", "/images/../json"),
            ("GET", "/images//etc/passwd/json"),
            ("GET", "/images/x y/json"),
            ("GET", "/distribution/x/../json"),
            ("GET", "/containers/bot/jsonx"),
            ("GET", "/v1.43/../containers/json"),
            ("GET", "/containers/bot\n/json"),
        ];
        for (m, t) in denied {
            assert!(classify(m, t).is_none(), "{m} {t} must be denied");
        }
    }

    // ---- authorization with a fake Docker ------------------------------------

    struct FakeDocker {
        /// name -> inspect body
        containers: Map<String, Value>,
        sent: std::sync::Mutex<Vec<String>>,
    }

    impl FakeDocker {
        fn new() -> Arc<Self> {
            let mk = |name: &str, id: &str, image: &str| {
                json!({
                    "Id": id, "Name": format!("/{name}"), "Image": format!("sha256:{id}"),
                    "State": {"Status": "running"},
                    "Config": {"Image": image, "Env": ["SUPABASE_SECRET_KEY=sekrit", "DISCORD_TOKEN=tok"]},
                })
            };
            let mut containers = Map::new();
            let bot = mk("bot", "aaaa1111", "ghcr.io/c/bot:1");
            let db = mk("secretdb", "bbbb2222", "postgres:17");
            containers.insert("bot".into(), bot.clone());
            containers.insert("aaaa1111".into(), bot);
            containers.insert("secretdb".into(), db.clone());
            containers.insert("bbbb2222".into(), db);
            Arc::new(Self {
                containers,
                sent: Default::default(),
            })
        }

        fn reply(&self, target: &str) -> io::Result<HttpResponse> {
            self.sent.lock().unwrap().push(target.to_string());
            let ok = |v: Value| HttpResponse {
                status: 200,
                content_type: Some("application/json".into()),
                body: v.to_string().into_bytes(),
            };
            let none = HttpResponse {
                status: 404,
                content_type: None,
                body: b"{}".to_vec(),
            };
            let path = target.split('?').next().unwrap();
            if path == "/containers/json" {
                return Ok(ok(json!([
                    {"Id": "aaaa1111", "Names": ["/bot"]},
                    {"Id": "bbbb2222", "Names": ["/secretdb"]},
                    {"Id": "cccc3333", "Names": ["/bot-sidecar"]},
                ])));
            }
            if let Some(rest) = path.strip_prefix("/containers/") {
                let (id, verb) = rest.split_once('/').unwrap();
                return Ok(match self.containers.get(id) {
                    Some(c) if verb == "json" => ok(c.clone()),
                    Some(_) => ok(json!({"verb": verb})),
                    None => none,
                });
            }
            if path.starts_with("/images/") {
                return Ok(ok(
                    json!({"RepoDigests": [], "Config": {"Env": ["LEAK=1"]}}),
                ));
            }
            if path.starts_with("/distribution/") {
                return Ok(ok(
                    json!({"Descriptor": {"digest": "sha256:deadbeefdeadbeef"}}),
                ));
            }
            Ok(none)
        }
    }

    struct Up(Arc<FakeDocker>);
    impl Upstream for Up {
        async fn get(&self, t: &str) -> io::Result<HttpResponse> {
            self.0.reply(t)
        }
        async fn post(&self, t: &str) -> io::Result<HttpResponse> {
            self.0.reply(t)
        }
    }

    fn allowed() -> Vec<String> {
        vec!["bot".into()]
    }

    #[tokio::test]
    async fn unconfigured_containers_are_denied_on_inspect_logs_stats_and_start() {
        let docker = FakeDocker::new();
        let up = Up(docker.clone());
        for (m, t) in [
            ("GET", "/containers/secretdb/json"),
            ("GET", "/containers/bbbb2222/json"),
            ("GET", "/containers/secretdb/logs?stdout=1"),
            ("GET", "/containers/bbbb2222/stats?stream=false"),
            ("POST", "/containers/secretdb/start"),
            ("POST", "/containers/secretdb/stop"),
            ("POST", "/containers/bbbb2222/restart"),
            ("GET", "/containers/does-not-exist/json"),
        ] {
            let r = serve(&allowed(), &up, m, t).await;
            assert_eq!(r.status, 403, "{m} {t}");
            assert!(!String::from_utf8_lossy(&r.body).contains("sekrit"));
        }
        // And nothing but lookups ever reached Docker for them.
        let sent = docker.sent.lock().unwrap();
        assert!(
            sent.iter().all(|t| t.ends_with("/json")),
            "only inspect lookups may reach docker: {sent:?}"
        );
    }

    #[tokio::test]
    async fn configured_containers_resolve_by_name_or_id_and_are_forwarded_by_full_id() {
        let docker = FakeDocker::new();
        let up = Up(docker.clone());
        for id in ["bot", "aaaa1111"] {
            let r = serve(
                &allowed(),
                &up,
                "POST",
                &format!("/containers/{id}/restart?t=5"),
            )
            .await;
            assert_eq!(r.status, 200, "{id}");
        }
        assert!(
            docker
                .sent
                .lock()
                .unwrap()
                .contains(&"/containers/aaaa1111/restart?t=5".to_string())
        );
        let r = serve(&allowed(), &up, "GET", "/containers/bot/stats?stream=false").await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn inspect_never_returns_config_env() {
        let up = Up(FakeDocker::new());
        let r = serve(&allowed(), &up, "GET", "/containers/bot/json").await;
        assert_eq!(r.status, 200);
        let text = String::from_utf8(r.body.clone()).unwrap();
        assert!(
            !text.contains("SUPABASE_SECRET_KEY") && !text.contains("sekrit"),
            "{text}"
        );
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v["Config"].get("Env").is_none());
        assert_eq!(v["Config"]["Image"], "ghcr.io/c/bot:1");
    }

    #[tokio::test]
    async fn the_container_list_is_filtered_to_configured_names() {
        let up = Up(FakeDocker::new());
        let r = serve(&allowed(), &up, "GET", "/containers/json").await;
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v, json!([{"Id": "aaaa1111", "Names": ["/bot"]}]));
    }

    #[tokio::test]
    async fn images_are_limited_to_configured_ones() {
        let up = Up(FakeDocker::new());
        for (t, status) in [
            ("/images/ghcr.io/c/bot:1/json", 200),
            ("/images/sha256:aaaa1111/json", 200),
            ("/distribution/ghcr.io/c/bot:1/json", 200),
            ("/images/postgres:17/json", 403),
            ("/images/sha256:bbbb2222/json", 403),
            ("/distribution/postgres:17/json", 403),
        ] {
            assert_eq!(serve(&allowed(), &up, "GET", t).await.status, status, "{t}");
        }
        let r = serve(&allowed(), &up, "GET", "/images/ghcr.io/c/bot:1/json").await;
        assert!(!String::from_utf8_lossy(&r.body).contains("LEAK"));
    }

    #[tokio::test]
    async fn a_missing_configured_container_is_404_not_403() {
        let up = Up(FakeDocker::new());
        let allowed = vec!["bot".to_string(), "gone".to_string()];
        assert_eq!(
            serve(&allowed, &up, "POST", "/containers/gone/start")
                .await
                .status,
            404
        );
        assert_eq!(
            serve(&allowed, &up, "POST", "/containers/other/start")
                .await
                .status,
            403
        );
    }

    // ---- client against a fake proxy ------------------------------------------

    fn jb(v: Value) -> Vec<u8> {
        v.to_string().into_bytes()
    }

    #[tokio::test]
    async fn status_maps_inspect_and_stats() {
        let dir = tmpdir("status");
        let sock = fake_socket(
            &dir,
            vec![
                (
                    "/containers/bot/json",
                    200,
                    "application/json",
                    jb(json!({
                        "Created": "2026-10-01T00:00:00Z",
                        "RestartCount": 2,
                        "State": {"Status": "running", "StartedAt": "2026-10-22T09:01:20.123456789Z", "FinishedAt": "0001-01-01T00:00:00Z"},
                        "HostConfig": {"Memory": 2147483648u64},
                    })),
                ),
                (
                    "/containers/bot/stats?stream=false",
                    200,
                    "application/json",
                    jb(json!({
                        "cpu_stats": {"cpu_usage": {"total_usage": 300}, "system_cpu_usage": 2000},
                        "precpu_stats": {"cpu_usage": {"total_usage": 100}, "system_cpu_usage": 1000},
                        "memory_stats": {"usage": 1000, "stats": {"inactive_file": 400}},
                    })),
                ),
            ],
        )
        .await;
        let c = DockerClient::new(sock);
        let s = c.status("bot").await.unwrap();
        assert_eq!(
            s,
            DockerStatus {
                state: "up",
                since: Some("2026-10-22T09:01:20Z".into()),
                last_restart: Some("2026-10-22T09:01:20Z".into()),
                cpu_pct: 20.0,
                mem_bytes: 600,
                mem_limit_bytes: 2147483648,
            }
        );
        assert!(matches!(c.status("nope").await, Err(DockerError::NotFound)));
    }

    #[test]
    fn state_mapping() {
        for (status, state) in [
            ("running", "up"),
            ("restarting", "restarting"),
            ("exited", "down"),
            ("created", "down"),
            ("paused", "down"),
            ("weird", "unknown"),
        ] {
            let s = status_from_inspect(&json!({"State": {"Status": status}}));
            assert_eq!(s.state, state, "{status}");
        }
    }

    #[tokio::test]
    async fn unreachable_proxy_is_unavailable() {
        let c = DockerClient::new("/nonexistent/t27.sock");
        let e = c.inspect("bot").await.unwrap_err();
        assert!(matches!(e, DockerError::Unavailable(_)));
        assert_eq!(e.status(), 503);
    }

    #[tokio::test]
    async fn logs_demultiplex_and_come_newest_first() {
        let mut body = Vec::new();
        for (stream, text) in [
            (1u8, "2026-10-22T09:00:01.5Z first\n"),
            (2u8, "2026-10-22T09:00:02.0Z second\n"),
        ] {
            body.extend([stream, 0, 0, 0]);
            body.extend((text.len() as u32).to_be_bytes());
            body.extend(text.as_bytes());
        }
        let dir = tmpdir("logs");
        let sock = fake_socket(
            &dir,
            vec![(
                "/containers/bot/logs?stdout=1&stderr=1&timestamps=1&tail=2&until=1792659602",
                200,
                "application/vnd.docker.multiplexed-stream",
                body,
            )],
        )
        .await;
        let lines = DockerClient::new(sock)
            .logs("bot", Some(1792659601), 2)
            .await
            .unwrap();
        let msgs: Vec<_> = lines.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(msgs, ["second", "first"]);
        assert_eq!(lines[1].timestamp, 1_792_659_601_500);
    }

    #[test]
    fn plain_tty_logs_parse_too() {
        let l = parse_logs(b"2026-10-22T09:00:01Z hello world\nno timestamp here\n");
        assert_eq!(l[0].message, "hello world");
        assert_eq!(l[1].message, "no timestamp here");
    }

    fn image_routes(
        registry: Option<&'static str>,
    ) -> Vec<(&'static str, u16, &'static str, Vec<u8>)> {
        let mut r = vec![
            (
                "/containers/bot/json",
                200,
                "application/json",
                jb(json!({"Image": "sha256:img1", "Config": {"Image": "ghcr.io/c/bot:1.4.2"}})),
            ),
            (
                "/images/sha256:img1/json",
                200,
                "application/json",
                jb(json!({"RepoDigests": ["ghcr.io/c/bot@sha256:1234567890abcdef"]})),
            ),
        ];
        if let Some(d) = registry {
            r.push((
                "/distribution/ghcr.io/c/bot:1.4.2/json",
                200,
                "application/json",
                jb(json!({"Descriptor": {"digest": d}})),
            ));
        } else {
            r.push((
                "/distribution/ghcr.io/c/bot:1.4.2/json",
                500,
                "application/json",
                jb(json!({"message": "registry down"})),
            ));
        }
        r
    }

    #[tokio::test]
    async fn image_reports_current_and_latest_short_digests() {
        let dir = tmpdir("image");
        let sock = fake_socket(&dir, image_routes(Some("sha256:fedcba0987654321"))).await;
        let info = DockerClient::new(sock).image("bot").await.unwrap();
        assert_eq!(info.current, "ghcr.io/c/bot:1.4.2@12345678");
        assert_eq!(info.latest, "ghcr.io/c/bot:1.4.2@fedcba09");
        assert!(DateTime::parse_from_rfc3339(&info.checked_at).is_ok());
    }

    #[tokio::test]
    async fn image_without_repo_digests_reads_as_up_to_date() {
        let dir = tmpdir("local");
        let sock = fake_socket(
            &dir,
            vec![
                (
                    "/containers/bot/json",
                    200,
                    "application/json",
                    jb(json!({"Image": "sha256:img1", "Config": {"Image": "bot"}})),
                ),
                (
                    "/images/sha256:img1/json",
                    200,
                    "application/json",
                    jb(json!({"RepoDigests": []})),
                ),
            ],
        )
        .await;
        let info = DockerClient::new(sock).image("bot").await.unwrap();
        assert_eq!(info.current, "bot:latest@unknown");
        assert_eq!(info.latest, info.current);
    }

    #[tokio::test]
    async fn unreachable_registry_serves_cache_else_equals_current() {
        // No cache: latest == current.
        let dir = tmpdir("reg-none");
        let sock = fake_socket(&dir, image_routes(None)).await;
        let c = DockerClient::new(sock);
        let info = c.image("bot").await.unwrap();
        assert_eq!(info.latest, info.current);

        // With a stale cache entry: the cached value and its time.
        c.cache.lock().unwrap().insert(
            "ghcr.io/c/bot:1.4.2".into(),
            (
                "cafebabe".into(),
                "2026-10-22T11:51:20Z".parse().unwrap(),
                Instant::now() - IMAGE_CACHE_TTL - Duration::from_secs(1),
            ),
        );
        let info = c.image("bot").await.unwrap();
        assert_eq!(info.latest, "ghcr.io/c/bot:1.4.2@cafebabe");
        assert_eq!(info.checked_at, "2026-10-22T11:51:20Z");
    }

    #[tokio::test]
    async fn a_fresh_cache_entry_is_not_looked_up_again() {
        let dir = tmpdir("reg-fresh");
        // The registry route is absent (404): a lookup would fall back to "current".
        let mut routes = image_routes(Some("sha256:fedcba0987654321"));
        routes.pop();
        let sock = fake_socket(&dir, routes).await;
        let c = DockerClient::new(sock);
        c.cache.lock().unwrap().insert(
            "ghcr.io/c/bot:1.4.2".into(),
            (
                "cafebabe".into(),
                "2026-10-22T11:51:20Z".parse().unwrap(),
                Instant::now(),
            ),
        );
        let info = c.image("bot").await.unwrap();
        assert_eq!(info.latest, "ghcr.io/c/bot:1.4.2@cafebabe");
        c.forget_images();
        let info = c.image("bot").await.unwrap();
        assert_eq!(info.latest, info.current);
    }

    #[test]
    fn chunked_bodies_decode() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: a/b\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"abcde"[..]));
        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn update_env_files_are_generated_from_config() {
        let config = Config::parse(
            r#"
[[service]]
id = "discord-bot"
name = "Bot"
backend = "docker"
container = "chulacraft-discord"
compose_file = "/srv/chulacraft/discord/compose.yaml"
compose_service = "bot"

[[service]]
id = "plain"
name = "Plain"
backend = "docker"
container = "plain"
"#,
        )
        .unwrap();
        let dir = tmpdir("env");
        std::fs::write(dir.join("stale.env"), "COMPOSE_FILE=x").unwrap();
        let written = write_update_envs(&config, &dir).unwrap();
        assert_eq!(written, ["discord-bot.env"]);
        assert_eq!(
            std::fs::read_to_string(dir.join("discord-bot.env")).unwrap(),
            "COMPOSE_FILE=\"/srv/chulacraft/discord/compose.yaml\"\nCOMPOSE_SERVICE=\"bot\"\n"
        );
        assert!(!dir.join("stale.env").exists());
        assert!(!dir.join("plain.env").exists());
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(dir.join("discord-bot.env"))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);

        let mut bad = config.services[0].clone();
        bad.compose_service = Some("bot\nEVIL=1".into());
        assert!(update_env(&bad).is_err());
        bad.compose_service = Some("a\"b".into());
        assert!(update_env(&bad).is_err());
    }

    #[test]
    fn image_reference_helpers() {
        assert_eq!(with_tag("bot"), "bot:latest");
        assert_eq!(with_tag("localhost:5000/bot"), "localhost:5000/bot:latest");
        assert_eq!(with_tag("a/b:2"), "a/b:2");
        assert_eq!(repo_of("localhost:5000/bot:2"), "localhost:5000/bot");
        assert_eq!(repo_of("a/b@sha256:ff"), "a/b");
        assert_eq!(short_digest("sha256:0123456789"), Some("01234567".into()));
        assert_eq!(short_digest("md5:0123456789"), None);
    }
}
