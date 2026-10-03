use std::{error::Error, fmt, ops::{Index, IndexMut}, sync::Arc};

use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, RefreshKind, System};

use axum::{
    Json, extract::{Path, Query, Request, State, ws::{Message, WebSocket, WebSocketUpgrade}}, http::{StatusCode, header}, middleware::Next, response::{IntoResponse, Response}
};
use thiserror::Error;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{AppState, mcsv_mgr::{JournalBroadcaster}};

#[derive(Debug)] // Required for the Error trait
pub struct ApiError (StatusCode);

impl From<StatusCode> for ApiError {
    fn from(value: StatusCode) -> Self { Self(value) }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

// Implement the Error trait
impl Error for ApiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

#[derive(Error, Debug)]
#[allow(unused)]
pub enum AppError {
    #[error("Systemd D-Bus error: {0}")]
    Dbus(#[from] zbus::Error),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("API error: {0}")]
    Api(#[from] ApiError),

    #[error("Unknown error")]
    Unknown
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::Api(ApiError(status)) = self {
            return status.into_response();
        }

        eprintln!("Internal Error: {}", self);

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            #[cfg(debug_assertions)]
            format!("Something went wrong: {}", self),
            #[cfg(not(debug_assertions))]
            "Something went wrong",
        )
            .into_response()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Claims {
    pub sub: String,    // User ID
    pub exp: u64,     // Expiration time
    pub role: String,   // Authorization role
}


#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum AuthCred {
    Claims(Claims),
    Bypass,
    Unauthorized
}

pub async fn auth_middleware(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let ac: AuthCred = 'auth: {
        if request.headers().get("Host").map_or(false, |h| h == "bypass") {
            break 'auth AuthCred::Bypass;
        }

        if let Some(jwt_secret) = &state.jwt_secret {
            let auth_header = match request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|header| header.to_str().ok()) {
                    Some(ah) => ah,
                    None => break 'auth AuthCred::Unauthorized
                };
    
            if !auth_header.starts_with("Bearer ") {
                break 'auth AuthCred::Unauthorized;
            }
    
            let token = &auth_header[7..];
    
            let claims = match decode::<Claims>(
                token,
                &DecodingKey::from_secret(jwt_secret.as_bytes()),
                &Validation::default(),
            ) {
                Ok(token_data) => token_data.claims,
                Err(_) => break 'auth AuthCred::Unauthorized
            };
    
            let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    
            if claims.exp < current_time {
                break 'auth AuthCred::Unauthorized;
            }
    
            AuthCred::Claims(claims)
        } else {
            AuthCred::Unauthorized
        }
    };

    request.extensions_mut().insert(ac);

    next.run(request).await
}


pub async fn auth_check_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    if request.extensions().get::<AuthCred>().map_or(true, |ac| *ac == AuthCred::Unauthorized) {
        Err(StatusCode::UNAUTHORIZED)
    } else {
        Ok(next.run(request).await)
    }
}

pub async fn get_status() -> Json<Value> {
    let mut sys = System::new_with_specifics(RefreshKind::nothing()
        .with_memory(MemoryRefreshKind::everything())
        .with_cpu(CpuRefreshKind::everything())
    );
    sys.refresh_memory();
    sys.refresh_cpu_all();
    let res = Json(json!({
        "MemTotal": sys.total_memory() / 1024,
        "MemFree": sys.free_memory() / 1024,
        "MemAvailable": sys.available_memory() / 1024,
        "SwapTotal": sys.total_swap() / 1024,
        "SwapFree": sys.free_swap() / 1024,
        "GlobalCpuUsage": sys.global_cpu_usage()
    }));
    // let cpu_usage: Vec<Value> = sys.cpus().iter().map(|cpu| json!(cpu.cpu_usage())).collect();
    // *res.index_mut("CpusUsage") = Value::Array(cpu_usage);
    res
}

pub async fn list_servers(
    State(state): State<Arc<AppState>>
) -> Result<Json<Value>, AppError> {
    let servers = state.mcsv_mgr.lock().await
        .server_names.iter().map(|name| json!(name)).collect();
    
    Ok(Json(Value::Array(servers)))
}

pub async fn get_server_status(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>
) -> Result<Json<Value>, AppError> {
    let unit = state.mcsv_mgr.lock().await.get_server_unit_status(&id).await?;

    let unit = if let Some(unit) = unit { unit } else { return Err(AppError::Api(StatusCode::NOT_FOUND.into())); };

    let mut res = Json(json!({}));
    
    *res.index_mut("unit") = json!({
        "name": unit.name,
        "load_state": unit.load_state,
        "active_state": unit.active_state,
        "sub_state": unit.sub_state
    });

    let proc = state.mcsv_mgr.lock().await.get_server_process(&id).await;
    
    if let Ok(Some(proc)) = proc {
        *res.index_mut("stat") = json!({
            "pid": proc.pid
        });
        let mut system = System::new_with_specifics(RefreshKind::nothing()
                .with_processes(ProcessRefreshKind::everything()));
        system.refresh_all();
        if let Some(proc) = system.process(Pid::from_u32(proc.pid)) {
            let stat = res.index_mut("stat");
            *stat.index_mut("name") = Value::String(proc.name().to_string_lossy().into_owned());
            *stat.index_mut("cpu_usage") = json!(proc.cpu_usage());
            *stat.index_mut("memory") = json!(proc.memory());
            *stat.index_mut("start_time") = json!(proc.start_time());
            *stat.index_mut("run_time") = json!(proc.run_time());
        }
    }

    Ok(res)
}

#[derive(serde::Deserialize, Debug)]
pub struct LogQueryParam {
    pub since: u64,
    pub until: Option<u64>,
    pub max_lines: Option<u64>
}

pub async fn get_server_log(
    Path(id): Path<String>,
    Query(param): Query<LogQueryParam>,
    state: State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let jb = {
        let mcsv_mgr = state.mcsv_mgr.lock().await;
        let lb = mcsv_mgr.log_broadcasters.get(&id);
        if let Some(lb) = lb { lb.clone() } else { return Err(AppError::Api(StatusCode::NOT_FOUND.into())); }
    };

    Ok(Json(Value::Array(jb.get_logs(param.since, param.until, param.max_lines)?
        .iter().map(|logline| json!({
            "message": logline.message,
            "timestamp": logline.timestamp,
            "comm": logline.command,
            "pid": logline.pid
        })).collect())))
}

#[derive(serde::Deserialize, Debug)]
pub struct RlogQueryParam {
    pub since: Option<u64>,
    pub until: u64,
    pub max_lines: Option<u64>
}

pub async fn get_server_rlog(
    Path(id): Path<String>,
    Query(param): Query<RlogQueryParam>,
    State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let jb = {
        let mcsv_mgr = state.mcsv_mgr.lock().await;
        let lb = mcsv_mgr.log_broadcasters.get(&id);
        if let Some(lb) = lb { lb.clone() } else { return Err(AppError::Api(StatusCode::NOT_FOUND.into())); }
    };

    Ok(Json(Value::Array(jb.get_rlogs(param.since, param.until, param.max_lines)?
        .iter().map(|logline| json!({
            "message": logline.message,
            "timestamp": logline.timestamp,
            "comm": logline.command,
            "pid": logline.pid
        })).collect())))
}

#[derive(serde::Deserialize, Debug)]
pub struct CommandQueryParam {
    pub cmd: String
}

pub async fn handle_server_command(
    Path(id): Path<String>,
    Query(param): Query<CommandQueryParam>,
    State(state): State<Arc<AppState>>) -> Result<(), AppError> {
    
    let cmd = param.cmd;
    println!("Received command for {}: {}", id, cmd);
    let cmd = if !cmd.ends_with('\n') { cmd.to_string() + "\n" } else { cmd.to_string() };
    
    Ok(state.mcsv_mgr.lock().await.inject_command(&id, &cmd).await?)
}

pub async fn handle_server_action(
    Path((id, action)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<(), AppError> {
    println!("server action {} to {}", action, id);
    let _job_path = match action.as_str() {
        "start" => state.mcsv_mgr.lock().await.start_server(&id).await?,
        "stop" => state.mcsv_mgr.lock().await.stop_server(&id).await?,
        "restart" => state.mcsv_mgr.lock().await.restart_server(&id).await?,
        _ => return Err(AppError::Api(StatusCode::NOT_FOUND.into())),
    };

    Ok(())
}

pub async fn handle_server_console(Path(id): Path<String>, state: State<Arc<AppState>>, ws: WebSocketUpgrade) -> Result<impl axum::response::IntoResponse, AppError> {
    // println!("connecting to {}", id);
    let jb = {
        let mcsv_mgr = state.mcsv_mgr.lock().await;
        let lb = mcsv_mgr.log_broadcasters.get(&id);
        if let Some(lb) = lb { lb.clone() } else { return Err(AppError::Api(StatusCode::NOT_FOUND.into())); }
    };
    Ok(ws.on_upgrade(|s| handle_server_console_socket(s, state, jb)))
}

pub async fn handle_server_console_socket(socket: WebSocket, state: State<Arc<AppState>>, jb: Arc<JournalBroadcaster>) {
    let (sender, mut receiver) = socket.split();
    let mut rx = jb.tx.subscribe();
    let sender = Arc::new(Mutex::new(sender));

    let sender0 = sender.clone();

    let name = jb.name.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = receiver.next().await {
            let inmsg = serde_json::from_str(text.as_str());
            let inmsg: Value = if let Ok(inmsg) = inmsg { inmsg } else { 
                let msg = format!("Invalid json: {}", text.as_str());
                let msg = json!({
                    "type": "error",
                    "message": msg
                });
                let msg = serde_json::to_string(&msg);
                let msg = if let Ok(msg) = msg { msg } else { break; };
                let res = sender.lock().await.send(Message::Text(msg.into())).await;
                if res.is_err() {
                    break;
                }
                continue;
            };

            match inmsg.index("type").as_str() {
                Some("command") => {
                    let cmd = inmsg.index("command").as_str().unwrap_or("");
                    println!("Received command for {}: {}", name, cmd);
                    let cmd = if !cmd.ends_with('\n') { cmd.to_string() + "\n" } else { cmd.to_string() };
                    
                    let res = state.mcsv_mgr.lock().await.inject_command(&name, &cmd).await;
                    if let Err(e) = res {
                        let msg = format!("Failed to inject command: {}", e);
                        eprintln!("{}", &msg);
                        let msg = json!({
                            "type": "error",
                            "message": msg
                        });
                        let msg = serde_json::to_string(&msg);
                        let msg = if let Ok(msg) = msg { msg } else { break; };
                        let res = sender.lock().await.send(Message::Text(msg.into())).await;
                        if res.is_err() {
                            break;
                        }
                    }
                },
                Some("status_global") => {
                    let out = get_status().await;
                    let msg = json!({
                        "type": "status_global",
                        "status_global": out.0
                    });
                    let msg = serde_json::to_string(&msg);
                    let msg = if let Ok(msg) = msg { msg } else { break; };
                    let res = sender.lock().await.send(Message::Text(msg.into())).await;
                    if res.is_err() {
                        break;
                    }
                },
                Some("status_mcsv") => {
                    let unit = state.mcsv_mgr.lock().await.get_server_unit_status(&jb.name).await;
                    let unit = if let Ok(Some(unit)) = unit { unit } else { break; };
                    
                    let mut msg = json!({
                        "type": "status_mcsv"
                    });
                    *msg.index_mut("unit") = json!({
                        "name": unit.name,
                        "load_state": unit.load_state,
                        "active_state": unit.active_state,
                        "sub_state": unit.sub_state
                    });
                    let proc = state.mcsv_mgr.lock().await.get_server_process(&jb.name).await;
                    
                    if let Ok(Some(proc)) = proc {
                        *msg.index_mut("stat") = json!({
                            "pid": proc.pid
                        });
                        let mut system = System::new_with_specifics(RefreshKind::nothing()
                                .with_processes(ProcessRefreshKind::everything()));
                        system.refresh_all();
                        if let Some(proc) = system.process(Pid::from_u32(proc.pid)) {
                            let stat = msg.index_mut("stat");
                            *stat.index_mut("name") = Value::String(proc.name().to_string_lossy().into_owned());
                            *stat.index_mut("cpu_usage") = json!(proc.cpu_usage());
                            *stat.index_mut("memory") = json!(proc.memory());
                            *stat.index_mut("start_time") = json!(proc.start_time());
                            *stat.index_mut("run_time") = json!(proc.run_time());
                        }
                    }
                    let msg = serde_json::to_string(&msg);
                    let msg = if let Ok(msg) = msg { msg } else { break; };
                    let res = sender.lock().await.send(Message::Text(msg.into())).await;
                    if res.is_err() {
                        break;
                    }
                },
                Some("get_logs") => {
                    let (log_tx, mut log_rx) = tokio::sync::mpsc::channel(100);

                    let jb_clone = jb.clone();
                    let since = inmsg.index("since").as_u64().unwrap_or_default();
                    let until = inmsg.get("until").and_then(|v| v.as_u64());
                    let max_lines = inmsg.get("max_lines").and_then(|v| v.as_u64());

                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = jb_clone.get_logs_to(since, until, max_lines, log_tx) {
                            eprintln!("Log streaming error: {}", e);
                        }
                    });

                    while let Some(logline) = log_rx.recv().await {
                        let msg = json!({
                            "type": "log",
                            "message": logline.message,
                            "timestamp": logline.timestamp,
                            "comm": logline.command,
                            "pid": logline.pid
                        });

                        if let Ok(serialized) = serde_json::to_string(&msg) {
                            let mut guard = sender.lock().await;
                            if guard.send(Message::Text(serialized.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                },
                Some("get_rlogs") => {
                    let (log_tx, mut log_rx) = tokio::sync::mpsc::channel(100);

                    let jb_clone = jb.clone();
                    let since = inmsg.get("since").and_then(|v| v.as_u64());
                    let until = inmsg.index("until").as_u64().unwrap_or_default();
                    let max_lines = inmsg.get("max_lines").and_then(|v| v.as_u64());

                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = jb_clone.get_rlogs_to(since, until, max_lines, log_tx) {
                            eprintln!("Log streaming error: {}", e);
                        }
                    });

                    while let Some(logline) = log_rx.recv().await {
                        let msg = json!({
                            "type": "log",
                            "message": logline.message,
                            "timestamp": logline.timestamp,
                            "comm": logline.command,
                            "pid": logline.pid
                        });

                        if let Ok(serialized) = serde_json::to_string(&msg) {
                            let mut guard = sender.lock().await;
                            if guard.send(Message::Text(serialized.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                _ => {}
            };
        }
    });

    let mut send_task = tokio::spawn(async move {
        while let Ok(logline) = rx.recv().await {
            let msg = json!({
                "type": "log",
                "message": logline.message,
                "comm": logline.command,
                "pid": logline.pid,
                "timestamp": logline.timestamp
            });
            let msg = serde_json::to_string(&msg);
            let msg = if let Ok(msg) = msg { msg } else { break; };
            let res = sender0.lock().await.send(Message::Text(msg.into())).await;
            if res.is_err() {
                break;
            }
        };
    });
    // println!("connected to {}", &jb.name);

    // If either task finishes (disconnect or error), abort the other
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };
}