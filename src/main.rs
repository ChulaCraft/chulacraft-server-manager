use std::sync::Arc;

use mcsv_manager::{
    auth::{self, Peer},
    backend::ServiceBackend,
    config::Config,
    routes,
    state::AppState,
};
use tokio::{
    net::UnixListener,
    signal::unix::{SignalKind, signal},
};

#[cfg(not(feature = "fake-backend"))]
async fn backend() -> Arc<dyn ServiceBackend> {
    Arc::new(
        mcsv_manager::backend::SystemdBackend::new()
            .await
            .expect("cannot reach systemd over D-Bus"),
    )
}

#[cfg(feature = "fake-backend")]
async fn backend() -> Arc<dyn ServiceBackend> {
    Arc::new(mcsv_manager::backend::FakeBackend::new(&[
        "survival", "creative",
    ]))
}

enum Listener {
    Unix(UnixListener),
    #[cfg(feature = "fake-backend")]
    Tcp(tokio::net::TcpListener),
}

/// Production: the socket systemd hands over as fd 0.
#[cfg(not(feature = "fake-backend"))]
async fn listen() -> Listener {
    Listener::Unix(systemd_socket())
}

/// fake-backend: `MCSV_FAKE_LISTEN=tcp:127.0.0.1:8080` or `unix:/path/to.sock`.
#[cfg(feature = "fake-backend")]
async fn listen() -> Listener {
    let spec = std::env::var("MCSV_FAKE_LISTEN")
        .expect("MCSV_FAKE_LISTEN is required with fake-backend (tcp:ADDR or unix:PATH)");
    match spec.split_once(':') {
        Some(("tcp", addr)) => Listener::Tcp(tokio::net::TcpListener::bind(addr).await.unwrap()),
        Some(("unix", path)) => Listener::Unix(UnixListener::bind(path).unwrap()),
        _ => panic!("MCSV_FAKE_LISTEN must be tcp:ADDR or unix:PATH"),
    }
}

#[cfg(not(feature = "fake-backend"))]
fn systemd_socket() -> UnixListener {
    let std_listener: std::os::unix::net::UnixListener =
        unsafe { std::os::fd::FromRawFd::from_raw_fd(0) };
    std_listener.set_nonblocking(true).unwrap();
    UnixListener::from_std(std_listener).unwrap()
}

#[tokio::main]
async fn main() {
    let backend = backend().await;
    let jwt = auth::Jwt::from_env().unwrap_or_else(|e| panic!("{e}"));
    if jwt.is_none() {
        eprintln!("warning: MCSV_JWT_PUBLIC_KEY is not set: only root and trusted uids get in");
    }
    let state = Arc::new(AppState {
        backend: backend.clone(),
        config: Config::from_env().unwrap_or_else(|e| panic!("{e}")),
        jwt,
        trusted_uids: auth::trusted_uids_from_env().unwrap_or_else(|e| panic!("{e}")),
        game_uid: auth::game_uid(),
    });

    // Start the metrics sampler at boot so history accrues before anyone calls the API.
    mcsv_manager::metrics::ensure_started(&state);
    tokio::spawn(handle_signals(backend.clone()));
    backend.reload().await.unwrap();

    let app = routes::router(state);

    println!("Minecraft Server Manager listening...");

    match listen().await {
        Listener::Unix(l) => axum::serve(l, app.into_make_service_with_connect_info::<Peer>())
            .await
            .unwrap(),
        // No peer credentials over TCP, so every request takes the token path.
        #[cfg(feature = "fake-backend")]
        Listener::Tcp(l) => axum::serve(l, app).await.unwrap(),
    }
}

async fn handle_signals(backend: Arc<dyn ServiceBackend>) {
    // Create a listener for SIGHUP
    let mut reload_signal = signal(SignalKind::hangup()).unwrap();

    loop {
        reload_signal.recv().await;
        println!("Received SIGHUP: Reloading configuration...");

        backend.reload().await.unwrap();

        println!("Reload complete.");
    }
}
