#[cfg(all(feature = "fake-backend", not(debug_assertions)))]
compile_error!("the fake-backend feature is test-only and must not be built in release mode");

pub mod api;
pub mod auth;
pub mod backend;
pub mod backups;
pub mod config;
pub mod docker;
pub mod handlers;
pub mod mcsv_mgr;
pub mod metrics;
pub mod models;
pub mod routes;
pub mod state;
pub mod systemd1;
