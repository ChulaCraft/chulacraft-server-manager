//! The `/service*` and `/backups` API from docs/ops-manager-contract.md.

use std::sync::Arc;

use axum::Router;

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
