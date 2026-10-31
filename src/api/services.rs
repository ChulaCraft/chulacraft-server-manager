use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};

use super::not_implemented;
use crate::{
    auth::{self, Auth},
    handlers::AppError,
    state::AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/services", get(list))
        .route("/service/{id}/status", get(status))
        .route("/service/{id}/metrics", get(metrics))
        .route("/service/{id}/uptime", get(uptime))
        .route("/service/{id}/rlog", get(rlog))
        .route("/service/{id}/{action}", post(action))
}

type Stub = Result<(StatusCode, Json<Value>), AppError>;

async fn list(auth: Auth) -> Stub {
    auth.require(auth::STATUS)?;
    Ok(not_implemented())
}

async fn status(auth: Auth) -> Stub {
    auth.require(auth::STATUS)?;
    Ok(not_implemented())
}

async fn metrics(auth: Auth) -> Stub {
    auth.require(auth::STATUS)?;
    Ok(not_implemented())
}

async fn uptime(auth: Auth) -> Stub {
    auth.require(auth::STATUS)?;
    Ok(not_implemented())
}

async fn rlog(auth: Auth) -> Stub {
    auth.require(auth::LOGS)?;
    Ok(not_implemented())
}

async fn action(auth: Auth, Path((_, action)): Path<(String, String)>) -> Stub {
    auth.require(match action.as_str() {
        "start" => auth::START,
        "stop" => auth::STOP,
        "restart" => auth::RESTART,
        _ => {
            return Ok((StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))));
        }
    })?;
    Ok(not_implemented())
}
