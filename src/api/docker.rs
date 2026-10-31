use std::sync::Arc;

use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::Value;

use super::not_implemented;
use crate::{
    auth::{self, Auth},
    handlers::AppError,
    state::AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/service/{id}/image", get(image))
        .route("/service/{id}/update", post(update))
}

type Stub = Result<(StatusCode, Json<Value>), AppError>;

async fn image(auth: Auth) -> Stub {
    auth.require(auth::STATUS)?;
    Ok(not_implemented())
}

async fn update(auth: Auth) -> Stub {
    auth.require(auth::UPDATE)?;
    Ok(not_implemented())
}
