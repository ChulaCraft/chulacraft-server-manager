//! Against the real Docker daemon, through a running `mcsv-docker-proxy`.
//! Run by `tests/docker/run.sh`, which starts the throwaway containers
//! `t27-target` (configured, with a fake SUPABASE_SECRET_KEY) and `t27-other`
//! (not configured), and the proxy. Ignored in a plain `cargo test`.

use std::path::PathBuf;

use mcsv_manager::{
    config::Action,
    docker::{DockerClient, DockerError, http_request},
};

const TARGET: &str = "t27-target";
const OTHER: &str = "t27-other";
const SECRET: &str = "fake-t27-secret-value";

fn socket() -> PathBuf {
    std::env::var("T27_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target-docker/t27/proxy.sock")
        })
}

#[tokio::test]
#[ignore = "needs tests/docker/run.sh"]
async fn config_env_is_absent_from_inspect() {
    let res = http_request(&socket(), "GET", &format!("/containers/{TARGET}/json"))
        .await
        .unwrap();
    assert_eq!(res.status, 200);
    let text = String::from_utf8(res.body).unwrap();
    assert!(!text.contains("SUPABASE_SECRET_KEY"), "Env leaked");
    assert!(!text.contains(SECRET), "secret leaked");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v["Config"].get("Env").is_none());
    assert!(
        v["Config"]["Image"].is_string(),
        "the rest of the body is intact"
    );
}

#[tokio::test]
#[ignore = "needs tests/docker/run.sh"]
async fn unconfigured_container_is_denied_everywhere() {
    let c = DockerClient::new(socket());
    assert!(matches!(c.inspect(OTHER).await, Err(DockerError::Denied)));
    assert!(matches!(c.stats(OTHER).await, Err(DockerError::Denied)));
    assert!(matches!(
        c.logs(OTHER, None, 5).await,
        Err(DockerError::Denied)
    ));
    assert!(matches!(
        c.act(OTHER, Action::Start).await,
        Err(DockerError::Denied)
    ));
    assert!(matches!(
        c.act(OTHER, Action::Stop).await,
        Err(DockerError::Denied)
    ));
    // It is still running: nothing reached it.
    let direct = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", OTHER])
        .output();
    if let Ok(o) = direct {
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "true");
    }
    // The list shows only configured containers.
    let res = http_request(&socket(), "GET", "/containers/json?all=1")
        .await
        .unwrap();
    let names: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
    let all: Vec<_> = names
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["Names"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|n| n.as_str())
        })
        .collect();
    assert_eq!(all, [format!("/{TARGET}")]);
}

#[tokio::test]
#[ignore = "needs tests/docker/run.sh"]
async fn everything_else_is_forbidden() {
    for (m, t) in [
        ("POST", "/containers/create?name=x"),
        ("POST", "/containers/t27-target/exec"),
        ("DELETE", "/containers/t27-target"),
        ("GET", "/volumes"),
        ("GET", "/info"),
        ("POST", "/images/create?fromImage=alpine"),
        ("GET", "/images/mcsv-manager-dev:latest/json"),
    ] {
        let res = http_request(&socket(), m, t).await.unwrap();
        // The last one is not a configured image reference of t27-target unless it is its image.
        if t.starts_with("/images/") {
            assert!(matches!(res.status, 200 | 403), "{m} {t}: {}", res.status);
        } else {
            assert_eq!(res.status, 403, "{m} {t}");
        }
    }
}

#[tokio::test]
#[ignore = "needs tests/docker/run.sh"]
async fn status_stats_logs_actions_and_image_work_for_the_configured_container() {
    let c = DockerClient::new(socket());
    let s = c.status(TARGET).await.unwrap();
    assert_eq!(s.state, "up");
    assert!(s.since.is_some());
    assert!(s.cpu_pct >= 0.0 && s.cpu_pct <= 100.0);
    assert!(s.mem_bytes > 0, "{s:?}");
    let _ = c.logs(TARGET, None, 10).await.unwrap();

    c.act(TARGET, Action::Stop).await.unwrap();
    assert_eq!(c.status(TARGET).await.unwrap().state, "down");
    c.act(TARGET, Action::Start).await.unwrap();
    assert_eq!(c.status(TARGET).await.unwrap().state, "up");
    c.act(TARGET, Action::Restart).await.unwrap();
    assert_eq!(c.status(TARGET).await.unwrap().state, "up");

    // A local image: either no RepoDigests (`@unknown`) or a digest, and a registry
    // that does not hold it or cannot be reached. Either way it reads as up to date.
    let img = c.image(TARGET).await.unwrap();
    let digest = img.current.rsplit_once('@').unwrap().1;
    assert!(
        digest == "unknown" || (digest.len() == 8 && digest.bytes().all(|b| b.is_ascii_hexdigit())),
        "{img:?}"
    );
    assert_eq!(img.latest, img.current);

    assert!(matches!(
        c.status("t27-missing").await,
        Err(DockerError::Denied)
    ));
}
