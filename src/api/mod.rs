//! The `/service*` and `/backups` API from docs/ops-manager-contract.md.
//! Each submodule is a stub that answers 501 until its task fills it in.

use std::sync::Arc;

use axum::{Json, Router, http::StatusCode};
use serde_json::{Value, json};

use crate::state::AppState;

pub mod backups;
pub mod docker;
pub mod services;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .merge(services::routes())
        .merge(docker::routes())
        .merge(backups::routes())
}

pub(crate) fn not_implemented() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": "not implemented" })),
    )
}
