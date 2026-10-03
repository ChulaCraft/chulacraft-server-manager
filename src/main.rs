use axum::{
    routing::get,
    Router
};
use std::{os::unix::io::FromRawFd, sync::Arc};
use tokio::{net::UnixListener, signal::unix::{SignalKind, signal}, sync::Mutex};

mod handlers;
mod systemd1;
mod mcsv_mgr;
use systemd1::{Systemd1};

use crate::{handlers::{auth_check_middleware, auth_middleware}, mcsv_mgr::McsvManager};

#[derive(Debug, Clone)]
pub struct AppState {
    pub jwt_secret: Option<String>,
    pub dbus: Arc<Systemd1>,
    pub mcsv_mgr: Arc<Mutex<McsvManager>>,
}

#[tokio::main]
async fn main() {
    let jwt_secret = std::env::var("JWT_SECRET").ok();
    let dbus = Arc::new(Systemd1::new().await.unwrap());
    let app_state = Arc::new(AppState {
        jwt_secret,
        dbus: dbus.clone(),
        mcsv_mgr: Arc::new(Mutex::new(McsvManager::new(dbus.clone())))
    });

    let signal_state = app_state.clone();
    tokio::spawn(async move {
        handle_signals(signal_state).await;
    });

    app_state.mcsv_mgr.lock().await.update().await.unwrap();

    let mut papp = Router::new()
        .route("/server/{id}/console", get(handlers::handle_server_console))
        .route("/server/{id}/log", get(handlers::get_server_log))
        .route("/server/{id}/rlog", get(handlers::get_server_rlog))
        .route("/server/{id}/cmd", get(handlers::handle_server_command))
        .route("/server/{id}/{action}", get(handlers::handle_server_action));

    if app_state.jwt_secret.is_some() {
        papp = papp.route_layer(axum::middleware::from_fn(auth_check_middleware));
    } else {
        eprintln!("Warning: JWT_SECRET is not set.");
    }

    let app = Router::new()
        .route("/status", get(handlers::get_status))
        .route("/servers", get(handlers::list_servers))
        .route("/server/{id}/status", get(handlers::get_server_status))
        .merge(papp)
        .route_layer(axum::middleware::from_fn_with_state(app_state.clone(), auth_middleware))
        .with_state(app_state.clone());

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(0) };
    std_listener.set_nonblocking(true).unwrap();
    let listener = UnixListener::from_std(std_listener).unwrap();

    println!("Minecraft Server Manager listening on Systemd socket...");

    axum::serve(
        listener, 
        app.into_make_service()
    ).await.unwrap();
}

async fn handle_signals(state: Arc<AppState>) {
    // Create a listener for SIGHUP
    let mut reload_signal = signal(SignalKind::hangup()).unwrap();

    loop {
        reload_signal.recv().await;
        println!("Received SIGHUP: Reloading configuration...");

        state.mcsv_mgr.lock().await.update().await.unwrap();
        
        println!("Reload complete.");
    }
}