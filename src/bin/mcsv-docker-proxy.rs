//! Filtering Docker socket proxy (T2.7, decision D8). Serves only the calls in
//! docs/ops-manager-contract.md ("Docker socket proxy"), narrowed to the
//! containers named in the manager config. The policy lives in
//! `mcsv_manager::docker::proxy`.
//!
//!   mcsv-docker-proxy                       serve (env below)
//!   mcsv-docker-proxy update-env <dir>      write <dir>/<id>.env for updatable services
//!
//! Only root and the manager's uid (`MCSV_DOCKER_PROXY_UIDS`, default the `mcsv-mgr` user)
//! are answered, checked with SO_PEERCRED; anyone else who can reach the socket gets 403.
//!
//! Env: `MCSV_CONFIG` (manager config), `MCSV_DOCKER_PROXY_LISTEN` (default
//! `/run/mcsv-docker-proxy/docker.sock`), `MCSV_DOCKER_UPSTREAM` (default
//! `/var/run/docker.sock`).

use std::{
    os::unix::fs::PermissionsExt, path::PathBuf, process::ExitCode, sync::Arc, time::Duration,
};

use mcsv_manager::{
    config::Config,
    docker::{
        DEFAULT_PROXY_SOCKET, DEFAULT_UPSTREAM_SOCKET,
        proxy::{Reply, SocketUpstream, allowed_peer_uids, containers, peer_allowed, serve},
        write_update_envs,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};

const MAX_HEAD: usize = 16 * 1024;
const CONN_TIMEOUT: Duration = Duration::from_secs(60);

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Status",
    }
}

async fn handle(
    mut s: UnixStream,
    allowed: Arc<Vec<String>>,
    peers: Arc<Vec<u32>>,
    up: Arc<SocketUpstream>,
) {
    if !peer_allowed(s.peer_cred().ok().map(|c| c.uid()), &peers) {
        let body = b"forbidden by mcsv-docker-proxy";
        let out = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let _ = s.write_all(out.as_bytes()).await;
        let _ = s.shutdown().await;
        return;
    }
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
        if head.len() > MAX_HEAD {
            return;
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut parts = text.lines().next().unwrap_or("").split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let reply = if target.is_empty() {
        Reply {
            status: 400,
            content_type: "text/plain".into(),
            body: b"bad request".to_vec(),
        }
    } else {
        serve(&allowed, &*up, method, target).await
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    )
    .into_bytes();
    out.extend(reply.body);
    let _ = s.write_all(&out).await;
    let _ = s.shutdown().await;
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let allowed = Arc::new(containers(&config));
    if allowed.is_empty() {
        eprintln!("mcsv-docker-proxy: no docker services in the config; every call will be denied");
    }
    let listen =
        std::env::var("MCSV_DOCKER_PROXY_LISTEN").unwrap_or_else(|_| DEFAULT_PROXY_SOCKET.into());
    let upstream =
        std::env::var("MCSV_DOCKER_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM_SOCKET.into());
    let peers = Arc::new(allowed_peer_uids(
        std::env::var("MCSV_DOCKER_PROXY_UIDS").ok().as_deref(),
    )?);
    let _ = std::fs::remove_file(&listen);
    let listener = UnixListener::bind(&listen)?;
    // The proxy group (mcsv-docker, only the manager) reads and writes it; nobody else.
    std::fs::set_permissions(&listen, std::fs::Permissions::from_mode(0o660))?;
    let up = Arc::new(SocketUpstream(PathBuf::from(upstream)));
    let slots = Arc::new(Semaphore::new(64));
    eprintln!("mcsv-docker-proxy: listening on {listen} for {allowed:?}");
    loop {
        let (stream, _) = listener.accept().await?;
        let permit = slots.clone().acquire_owned().await?;
        let (allowed, peers, up) = (allowed.clone(), peers.clone(), up.clone());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(CONN_TIMEOUT, handle(stream, allowed, peers, up)).await;
            drop(permit);
        });
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => run().await,
        [cmd, dir] if cmd == "update-env" => Config::from_env()
            .and_then(|c| write_update_envs(&c, &PathBuf::from(dir)))
            .map(|written| {
                for f in written {
                    println!("{dir}/{f}");
                }
            })
            .map_err(Into::into),
        _ => {
            eprintln!("usage: mcsv-docker-proxy [update-env <dir>]");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcsv-docker-proxy: {e}");
            ExitCode::FAILURE
        }
    }
}
