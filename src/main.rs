use axum::{
    routing::{get, post},
    Router
};
use std::{os::unix::io::FromRawFd, sync::Arc};
use tokio::{net::UnixListener, signal::unix::{SignalKind, signal}, sync::Mutex};

mod auth;
mod handlers;
mod systemd1;
mod mcsv_mgr;
use systemd1::{Systemd1};

use crate::mcsv_mgr::McsvManager;

#[derive(Debug)]
pub struct AppState {
    pub dbus: Arc<Systemd1>,
    pub mcsv_mgr: Arc<Mutex<McsvManager>>,
    pub jwt: auth::Jwt,
    pub unix_gids: Vec<u32>
}

#[tokio::main]
async fn main() {
    let dbus = Arc::new(Systemd1::new().await.unwrap());
    let app_state = Arc::new(AppState {
        dbus: dbus.clone(),
        mcsv_mgr: Arc::new(Mutex::new(McsvManager::new(dbus.clone()))),
        jwt: auth::Jwt::from_env(),
        unix_gids: auth::unix_gids()
    });

    let signal_state = app_state.clone();
    tokio::spawn(async move {
        handle_signals(signal_state).await;
    });

    app_state.mcsv_mgr.lock().await.update().await.unwrap();

    let app = Router::new()
        .route("/status", get(handlers::get_host_status))
        .route("/servers", get(handlers::list_servers))
        .route("/server/{id}/status", get(handlers::get_server_status))
        .route("/server/{id}/console", get(handlers::handle_server_console))
        .route("/server/{id}/log", get(handlers::get_server_log))
        .route("/server/{id}/rlog", get(handlers::get_server_rlog))
        // POST, so a link or prefetch can never fire them.
        .route("/server/{id}/cmd", post(handlers::handle_server_command))
        .route("/server/{id}/{action}", post(handlers::handle_server_action))
        .with_state(app_state.clone());

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(0) };
    std_listener.set_nonblocking(true).unwrap();
    let listener = UnixListener::from_std(std_listener).unwrap();

    println!("Minecraft Server Manager listening on Systemd socket...");

    axum::serve(
        listener, 
        app.into_make_service_with_connect_info::<auth::Peer>()
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