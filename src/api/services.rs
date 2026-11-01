//! `GET /services`, status, metrics, uptime, rlog, console, and start/stop/restart
//! for systemd (T2.6) and docker (T2.7) services. Docker services have no console.

use std::{collections::HashMap, path::Path, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path as UrlPath, Query, State, ws::WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::{
    api::docker::update_unit,
    auth::{self, Auth},
    config::{Action, Backend, ServiceConfig},
    docker::{self, DockerClient, DockerError},
    handlers::{self, AppError},
    metrics::{self, CGROUP_ROOT},
    models::ProcessStat,
    state::AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/services", get(list))
        .route("/service/{id}/status", get(status))
        .route("/service/{id}/metrics", get(metrics))
        .route("/service/{id}/uptime", get(uptime))
        .route("/service/{id}/rlog", get(rlog))
        .route("/service/{id}/console", get(console))
        .route("/service/{id}/{action}", post(action))
}

type Res<T = Json<Value>> = Result<T, AppError>;

fn not_found(msg: &str) -> AppError {
    AppError::status(StatusCode::NOT_FOUND, msg)
}

fn bad_request(msg: String) -> AppError {
    AppError::status(StatusCode::BAD_REQUEST, msg)
}

fn rfc3339(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn action_name(a: Action) -> &'static str {
    match a {
        Action::Start => "start",
        Action::Stop => "stop",
        Action::Restart => "restart",
    }
}

fn find<'a>(state: &'a AppState, id: &str) -> Result<&'a ServiceConfig, AppError> {
    state
        .config
        .services
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| not_found("unknown service"))
}

/// The systemd instance behind a service; docker services are not manageable this way.
fn instance(svc: &ServiceConfig) -> Result<&str, AppError> {
    metrics::instance_of(svc).ok_or_else(|| not_found("service is not manageable"))
}

fn container(svc: &ServiceConfig) -> Result<&str, AppError> {
    svc.container
        .as_deref()
        .ok_or_else(|| not_found("service is not manageable"))
}

fn docker_error(e: DockerError) -> AppError {
    eprintln!("docker: {e}");
    match e {
        DockerError::Unavailable(_) => {
            AppError::status(StatusCode::SERVICE_UNAVAILABLE, "docker unavailable")
        }
        DockerError::NotFound => not_found("container not found"),
        _ => AppError::status(StatusCode::INTERNAL_SERVER_ERROR, "docker error"),
    }
}

/// `up | down | restarting | unknown` from systemd's `ActiveState`.
pub fn state_of(active_state: &str, restart_recent: bool) -> &'static str {
    match active_state {
        "active" | "reloading" => "up",
        "activating" => "restarting",
        "deactivating" if restart_recent => "restarting",
        "deactivating" | "inactive" | "failed" => "down",
        _ => "unknown",
    }
}

async fn list(auth: Auth, State(state): State<Arc<AppState>>) -> Res {
    auth.require(auth::STATUS)?;
    let world = state
        .config
        .backups
        .as_ref()
        .map(|b| b.world_instances.as_slice())
        .unwrap_or_default();
    let services: Vec<Value> = state
        .config
        .services
        .iter()
        .map(|s| {
            let instance = metrics::instance_of(s);
            json!({
                "id": s.id,
                "name": s.name,
                "kind": match s.backend { Backend::Systemd => "systemd", Backend::Docker => "docker" },
                "console": s.console(),
                "actions": s.actions().into_iter().map(action_name).collect::<Vec<_>>(),
                "backups": instance.is_some_and(|i| world.iter().any(|w| w == i)),
                "updatable": s.updatable(),
            })
        })
        .collect();
    Ok(Json(Value::Array(services)))
}

async fn status(
    auth: Auth,
    UrlPath(id): UrlPath<String>,
    State(state): State<Arc<AppState>>,
) -> Res {
    auth.require(auth::STATUS)?;
    let svc = find(&state, &id)?;
    let store = metrics::ensure_started(&state);
    let now = Utc::now().timestamp();
    if svc.backend == Backend::Docker {
        return docker_status(docker::client(), &store, svc, now).await;
    }
    let instance = instance(svc)?;
    let reading =
        metrics::read_service(state.backend.as_ref(), instance, Path::new(CGROUP_ROOT)).await?;

    let state_name = state_of(&reading.active_state, store.restart_recent(&id, now));
    let first_since = if state_name == "up" {
        match state.backend.server_process(instance).await {
            Ok(Some(p)) => ProcessStat::try_from(&p)
                .ok()
                .and_then(|s| i64::try_from(s.start_time).ok()),
            _ => None,
        }
    } else {
        None
    };
    let (since, last_restart) = store.observe(&id, state_name, now, first_since);

    let (cpu, mem, limit) = match (&reading.cgroup, state_name) {
        (Some(c), "up") => (
            store
                .latest(&id)
                .filter(|s| s.up)
                .map_or(0.0, |s| s.cpu_pct),
            c.mem_bytes,
            c.mem_limit_bytes,
        ),
        _ => (0.0, 0, 0),
    };
    Ok(Json(json!({
        "state": state_name,
        "since": rfc3339(since),
        "last_restart": last_restart.map(rfc3339),
        "cpu_pct": cpu,
        "mem_bytes": mem,
        "mem_limit_bytes": limit,
    })))
}

pub async fn docker_status(
    client: &DockerClient,
    store: &metrics::Store,
    svc: &ServiceConfig,
    now: i64,
) -> Res {
    let st = client.status(container(svc)?).await.map_err(docker_error)?;
    let (seen_since, seen_restart) = store.observe(&svc.id, st.state, now, None);
    let unix = |s: &Option<String>| {
        s.as_deref()
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .map(|t| t.timestamp())
    };
    // Docker's own timestamps win; the store covers a restart it saw happen.
    let since = unix(&st.since).unwrap_or(seen_since);
    let last_restart = unix(&st.last_restart).max(seen_restart);
    Ok(Json(json!({
        "state": st.state,
        "since": rfc3339(since),
        "last_restart": last_restart.map(rfc3339),
        "cpu_pct": st.cpu_pct,
        "mem_bytes": st.mem_bytes,
        "mem_limit_bytes": st.mem_limit_bytes,
    })))
}

/// An integer query parameter within `min..=max`, or `default` when absent.
fn ranged(q: &HashMap<String, String>, key: &str, default: i64, min: i64, max: i64) -> Res<i64> {
    let Some(raw) = q.get(key) else {
        return Ok(default);
    };
    raw.parse::<i64>()
        .ok()
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| bad_request(format!("{key} must be an integer from {min} to {max}")))
}

async fn metrics(
    auth: Auth,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Res {
    auth.require(auth::STATUS)?;
    find(&state, &id)?;
    let hours = ranged(&q, "hours", 24, 1, 168)?;
    let points = metrics::ensure_started(&state).metrics(&id, hours, Utc::now().timestamp());
    Ok(Json(Value::Array(
        points
            .iter()
            .map(|s| json!({ "t": rfc3339(s.t), "cpu_pct": s.cpu_pct, "mem_bytes": s.mem_bytes }))
            .collect(),
    )))
}

async fn uptime(
    auth: Auth,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Res {
    auth.require(auth::STATUS)?;
    find(&state, &id)?;
    let days = ranged(&q, "days", 7, 1, 7)?;
    let points = metrics::ensure_started(&state).uptime(&id, days, Utc::now().timestamp());
    Ok(Json(Value::Array(
        points
            .iter()
            .map(|p| json!({ "t": rfc3339(p.t), "up_ratio": p.up_ratio }))
            .collect(),
    )))
}

const DEFAULT_RLOG_LINES: u64 = 200;
const MAX_RLOG_LINES: u64 = 5000;

async fn rlog(
    auth: Auth,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Res {
    auth.require(auth::LOGS)?;
    let svc = find(&state, &id)?;
    let max_lines = match q.get("max_lines") {
        None => DEFAULT_RLOG_LINES,
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=MAX_RLOG_LINES).contains(n))
            .ok_or_else(|| bad_request(format!("max_lines must be 1 to {MAX_RLOG_LINES}")))?,
    };
    // The journal counts microseconds; `until` defaults to now.
    let until = q
        .get("until")
        .map(|v| {
            DateTime::parse_from_rfc3339(v)
                .map(|t| t.timestamp_micros())
                .map_err(|_| bad_request("until must be an RFC 3339 timestamp".into()))
        })
        .transpose()?;
    if svc.backend == Backend::Docker {
        return docker_rlog(docker::client(), svc, until, max_lines).await;
    }
    let instance = instance(svc)?;
    let until_usec =
        u64::try_from(until.unwrap_or_else(|| Utc::now().timestamp_micros())).unwrap_or(0);
    let source = state
        .backend
        .log_source(instance)
        .await
        .ok_or_else(|| not_found("no logs for this service"))?;
    let lines = source.get_rlogs(None, until_usec, Some(max_lines))?;
    Ok(Json(Value::Array(
        lines
            .iter()
            .map(|l| {
                json!({
                    "message": l.message,
                    "timestamp": l.timestamp,
                    "comm": l.command,
                    "pid": l.pid,
                })
            })
            .collect(),
    )))
}

/// Same lines as the systemd rlog; docker has no command name or pid.
pub async fn docker_rlog(
    client: &DockerClient,
    svc: &ServiceConfig,
    until_usec: Option<i64>,
    max_lines: u64,
) -> Res {
    // Docker's `until` counts seconds; its line timestamps are milliseconds.
    let until = until_usec.map(|u| u64::try_from(u / 1_000_000).unwrap_or(0));
    let lines = client
        .logs(container(svc)?, until, max_lines)
        .await
        .map_err(docker_error)?;
    Ok(Json(Value::Array(
        lines
            .iter()
            .map(|l| {
                json!({
                    "message": l.message,
                    "timestamp": l.timestamp * 1000,
                    "comm": "",
                    "pid": 0,
                })
            })
            .collect(),
    )))
}

async fn console(
    auth: Auth,
    UrlPath(id): UrlPath<String>,
    state: State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> Res<impl IntoResponse> {
    auth.require(auth::CONSOLE_READ)?;
    let svc = find(&state, &id)?;
    if !svc.console() {
        return Err(not_found("service has no console"));
    }
    let source = state
        .backend
        .log_source(instance(svc)?)
        .await
        .ok_or_else(|| not_found("no console for this service"))?;
    // The browser must get its subprotocol echoed back or it drops the socket.
    Ok(ws
        .protocols([auth::WS_PROTOCOL])
        .on_upgrade(|s| handlers::handle_server_console_socket(s, state, source, auth)))
}

async fn action(
    auth: Auth,
    UrlPath((id, action)): UrlPath<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Res<(StatusCode, Json<Value>)> {
    let (wanted, bit) = match action.as_str() {
        "start" => (Action::Start, auth::START),
        "stop" => (Action::Stop, auth::STOP),
        "restart" => (Action::Restart, auth::RESTART),
        _ => return Err(not_found("not found")),
    };
    auth.require(bit)?;
    let svc = find(&state, &id)?;
    if !svc.actions().contains(&wanted) {
        return Err(not_found("action not supported"));
    }
    let store = metrics::ensure_started(&state);
    let _running = store.begin_action(&id).ok_or_else(busy)?;
    let instance = if svc.backend == Backend::Docker {
        // Checked before anything is logged or run.
        container(svc)?;
        if svc.updatable() && state.backend.unit_active(&update_unit(&id)).await? {
            return Err(busy());
        }
        ""
    } else {
        let instance = instance(svc)?;
        let reading =
            metrics::read_service(state.backend.as_ref(), instance, Path::new(CGROUP_ROOT)).await?;
        if matches!(reading.active_state.as_str(), "activating" | "deactivating") {
            return Err(busy());
        }
        instance
    };

    match auth.claims() {
        Some(c) => println!(
            "service action {action} to {id:?} from {:?} (jti {:?})",
            c.sub, c.jti
        ),
        None => println!(
            "service action {action} to {id:?} via peer uid {:?}",
            auth.peer_uid()
        ),
    }
    if svc.backend == Backend::Docker {
        docker_act(docker::client(), &store, svc, wanted).await?;
        return Ok((StatusCode::ACCEPTED, Json(json!({}))));
    }
    match wanted {
        Action::Start => state.backend.start_server(instance).await?,
        Action::Stop => state.backend.stop_server(instance).await?,
        Action::Restart => {
            state.backend.restart_server(instance).await?;
            store.mark_restart(&id, Utc::now().timestamp());
        }
    }
    Ok((StatusCode::ACCEPTED, Json(json!({}))))
}

fn busy() -> AppError {
    AppError::status(StatusCode::CONFLICT, "an action is already running")
}

pub async fn docker_act(
    client: &DockerClient,
    store: &metrics::Store,
    svc: &ServiceConfig,
    wanted: Action,
) -> Result<(), AppError> {
    client
        .act(container(svc)?, wanted)
        .await
        .map_err(docker_error)?;
    if wanted == Action::Restart {
        store.mark_restart(&svc.id, Utc::now().timestamp());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        docker::test_support::{fake_socket, tmpdir},
    };

    fn svc() -> ServiceConfig {
        Config::parse(
            "[[service]]\nid='discord-bot'\nname='Bot'\nbackend='docker'\ncontainer='bot'\n",
        )
        .unwrap()
        .services
        .remove(0)
    }

    const J: &str = "application/json";

    #[tokio::test]
    async fn status_maps_container_state() {
        let inspect = br#"{"State":{"Status":"running","StartedAt":"2026-01-02T03:04:05Z"},
            "RestartCount":2,"HostConfig":{"Memory":1000}}"#
            .to_vec();
        let stats = br#"{"cpu_stats":{"cpu_usage":{"total_usage":50},"system_cpu_usage":1000},
            "precpu_stats":{"cpu_usage":{"total_usage":0},"system_cpu_usage":0},
            "memory_stats":{"usage":4096}}"#
            .to_vec();
        let sock = fake_socket(
            &tmpdir("svc-status"),
            vec![
                ("/containers/bot/json", 200, J, inspect),
                ("/containers/bot/stats?stream=false", 200, J, stats),
            ],
        )
        .await;
        let store = metrics::Store::in_memory();
        let Json(v) = docker_status(&DockerClient::new(sock), &store, &svc(), 1_800_000_000)
            .await
            .unwrap();
        assert_eq!(
            v,
            json!({"state": "up", "since": "2026-01-02T03:04:05Z",
                "last_restart": "2026-01-02T03:04:05Z",
                "cpu_pct": 5.0, "mem_bytes": 4096, "mem_limit_bytes": 1000})
        );
    }

    #[tokio::test]
    async fn status_errors_map_to_http_statuses() {
        let store = metrics::Store::in_memory();
        let down = DockerClient::new("/nonexistent");
        let err = docker_status(&down, &store, &svc(), 0).await.unwrap_err();
        assert!(
            matches!(err, AppError::Status { status, .. } if status == StatusCode::SERVICE_UNAVAILABLE)
        );
        let sock = fake_socket(&tmpdir("svc-404"), vec![]).await;
        let err = docker_status(&DockerClient::new(sock), &store, &svc(), 0)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Status { status, .. } if status == StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn rlog_has_the_systemd_shape_newest_first() {
        let mut body = Vec::new();
        for (t, m) in [
            ("2026-01-01T00:00:01Z", "one"),
            ("2026-01-01T00:00:02Z", "two"),
        ] {
            let line = format!("{t} {m}\n");
            body.extend([1, 0, 0, 0, 0, 0, 0, line.len() as u8]);
            body.extend(line.bytes());
        }
        let sock = fake_socket(
            &tmpdir("svc-rlog"),
            vec![(
                "/containers/bot/logs?stdout=1&stderr=1&timestamps=1&tail=5",
                200,
                "application/octet-stream",
                body,
            )],
        )
        .await;
        let Json(v) = docker_rlog(&DockerClient::new(sock), &svc(), None, 5)
            .await
            .unwrap();
        assert_eq!(v[0]["message"], "two");
        assert_eq!(v[1]["message"], "one");
        assert_eq!(v[0]["timestamp"], 1_767_225_602_000_000u64);
        assert!(v[0]["comm"].is_string() && v[0]["pid"].is_number());
    }

    #[tokio::test]
    async fn act_runs_and_restart_is_remembered() {
        let sock = fake_socket(
            &tmpdir("svc-act"),
            vec![
                ("/containers/bot/start", 204, J, vec![]),
                ("/containers/bot/restart", 204, J, vec![]),
            ],
        )
        .await;
        let (client, store) = (DockerClient::new(sock), metrics::Store::in_memory());
        docker_act(&client, &store, &svc(), Action::Start)
            .await
            .unwrap();
        assert!(!store.restart_recent("discord-bot", Utc::now().timestamp()));
        docker_act(&client, &store, &svc(), Action::Restart)
            .await
            .unwrap();
        assert!(store.restart_recent("discord-bot", Utc::now().timestamp()));
        // stop is not routed by the fake: the container read as missing.
        let err = docker_act(&client, &store, &svc(), Action::Stop)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Status { status, .. } if status == StatusCode::NOT_FOUND));
    }

    #[test]
    fn docker_services_have_no_console() {
        assert!(!svc().console());
    }
}
