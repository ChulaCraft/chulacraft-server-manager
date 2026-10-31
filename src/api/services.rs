//! `GET /services`, status, metrics, uptime, rlog, console, and start/stop/restart
//! for systemd services (T2.6). Docker services answer 503 here until T2.7.

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
    auth::{self, Auth},
    config::{Action, Backend, ServiceConfig},
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

/// The systemd instance behind a service; docker services are T2.7's.
fn instance(svc: &ServiceConfig) -> Result<&str, AppError> {
    if svc.backend == Backend::Docker {
        return Err(AppError::status(
            StatusCode::SERVICE_UNAVAILABLE,
            "docker services are not available",
        ));
    }
    metrics::instance_of(svc).ok_or_else(|| not_found("service is not manageable"))
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
    let instance = instance(svc)?;
    let store = metrics::ensure_started(&state);
    let now = Utc::now().timestamp();
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
    let instance = instance(svc)?;
    let max_lines = match q.get("max_lines") {
        None => DEFAULT_RLOG_LINES,
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=MAX_RLOG_LINES).contains(n))
            .ok_or_else(|| bad_request(format!("max_lines must be 1 to {MAX_RLOG_LINES}")))?,
    };
    // The journal counts microseconds; `until` defaults to now.
    let until_usec = match q.get("until") {
        None => Utc::now().timestamp_micros(),
        Some(v) => DateTime::parse_from_rfc3339(v)
            .map_err(|_| bad_request("until must be an RFC 3339 timestamp".into()))?
            .timestamp_micros(),
    };
    let until_usec = u64::try_from(until_usec).unwrap_or(0);
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
    let instance = instance(svc)?;
    let store = metrics::ensure_started(&state);
    let busy = || AppError::status(StatusCode::CONFLICT, "an action is already running");
    let _running = store.begin_action(&id).ok_or_else(busy)?;
    let reading =
        metrics::read_service(state.backend.as_ref(), instance, Path::new(CGROUP_ROOT)).await?;
    if matches!(reading.active_state.as_str(), "activating" | "deactivating") {
        return Err(busy());
    }

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
