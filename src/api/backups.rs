//! `GET`/`POST /backups` (T2.9). Logic lives in `crate::backups`.
use std::sync::Arc;

use axum::{Json, Router, body::Bytes, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};

use crate::{
    auth::{self, Auth},
    backups::{self, Dirs, RunError},
    handlers::AppError,
    state::AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/backups", get(list).post(run))
}

async fn list(auth: Auth, State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    auth.require(auth::BACKUP_READ)?;
    let entries = backups::list(&Dirs::from_env(), state.backend.as_ref()).await?;
    Ok(Json(Value::Array(entries)))
}

async fn run(
    auth: Auth,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    auth.require(auth::BACKUP_RUN)?;
    let bad = || {
        AppError::status(
            StatusCode::BAD_REQUEST,
            "body must be {\"target\": \"world\"|\"supabase\"|\"bot\"}",
        )
    };
    let body: Value = serde_json::from_slice(&body).map_err(|_| bad())?;
    let target = body.get("target").and_then(Value::as_str).ok_or_else(bad)?;
    match backups::request(&Dirs::from_env(), state.backend.as_ref(), target).await {
        Ok(id) => Ok((StatusCode::ACCEPTED, Json(json!({ "id": id })))),
        Err(RunError::UnknownTarget) => Err(bad()),
        Err(RunError::Running) => Err(AppError::status(
            StatusCode::CONFLICT,
            "a backup of that target is already running",
        )),
        Err(RunError::App(e)) => Err(e),
    }
}
