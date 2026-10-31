use std::sync::Arc;

use crate::{auth::Jwt, backend::ServiceBackend, config::Config};

#[derive(Debug)]
pub struct AppState {
    pub backend: Arc<dyn ServiceBackend>,
    pub config: Config,
    pub jwt: Option<Jwt>,
    /// Uids (besides root) that skip the token on the unix socket. Never a group.
    pub trusted_uids: Vec<u32>,
    pub game_uid: Option<u32>,
}
