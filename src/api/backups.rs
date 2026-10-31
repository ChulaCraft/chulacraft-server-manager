use std::sync::Arc;

use axum::{Json, Router, http::StatusCode, routing::get};
use serde_json::Value;

use super::not_implemented;
use crate::{
    auth::{self, Auth},
    handlers::AppError,
    state::AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/backups", get(list).post(run))
}

type Stub = Result<(StatusCode, Json<Value>), AppError>;

async fn list(auth: Auth) -> Stub {
    auth.require(auth::BACKUP_READ)?;
    Ok(not_implemented())
}

async fn run(auth: Auth) -> Stub {
    auth.require(auth::BACKUP_RUN)?;
    Ok(not_implemented())
}
