//! `GET /service/:id/image` and `POST /service/:id/update` (T2.7).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{
    auth::{self, Auth},
    backend::ServiceBackend,
    config::{Backend, ServiceConfig},
    docker::{self, DockerClient, DockerError},
    handlers::AppError,
    state::AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/service/{id}/image", get(image))
        .route("/service/{id}/update", post(update))
}

type Reply = Result<(StatusCode, Json<Value>), AppError>;

fn error(status: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg })))
}

fn not_found() -> (StatusCode, Json<Value>) {
    error(StatusCode::NOT_FOUND, "not found")
}

/// The service when it exists and is an updatable docker service.
fn updatable<'a>(state: &'a AppState, id: &str) -> Option<&'a ServiceConfig> {
    state
        .config
        .services
        .iter()
        .find(|s| s.id == id && s.backend == Backend::Docker && s.updatable())
}

fn docker_error(e: DockerError) -> (StatusCode, Json<Value>) {
    eprintln!("docker: {e}");
    match e {
        DockerError::Unavailable(_) => error(StatusCode::SERVICE_UNAVAILABLE, "docker unavailable"),
        DockerError::NotFound => not_found(),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "docker error"),
    }
}

pub fn update_unit(id: &str) -> String {
    format!("chulacraft-update@{id}.service")
}

async fn image(auth: Auth, State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Reply {
    auth.require(auth::STATUS)?;
    let Some(svc) = updatable(&state, &id) else {
        return Ok(not_found());
    };
    Ok(image_reply(docker::client(), svc).await)
}

pub async fn image_reply(client: &DockerClient, svc: &ServiceConfig) -> (StatusCode, Json<Value>) {
    let container = svc.container.as_deref().unwrap_or_default();
    match client.image(container).await {
        Ok(info) => (StatusCode::OK, Json(json!(info))),
        Err(e) => docker_error(e),
    }
}

async fn update(auth: Auth, State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Reply {
    auth.require(auth::UPDATE)?;
    let Some(svc) = updatable(&state, &id) else {
        return Ok(not_found());
    };
    update_reply(&*state.backend, docker::client(), svc).await
}

/// Starts the root-owned update unit; the request only picks which instance.
pub async fn update_reply(
    backend: &dyn ServiceBackend,
    client: &DockerClient,
    svc: &ServiceConfig,
) -> Reply {
    let unit = update_unit(&svc.id);
    if backend.unit_active(&unit).await? {
        return Ok(error(StatusCode::CONFLICT, "update already running"));
    }
    backend.start_unit(&unit).await?;
    // The next `image` call must look the registry up again.
    client.forget_images();
    Ok((StatusCode::ACCEPTED, Json(json!({}))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::FakeBackend, config::Config};

    fn svc() -> ServiceConfig {
        Config::parse(
            r#"
[[service]]
id = "discord-bot"
name = "Bot"
backend = "docker"
container = "bot"
compose_file = "/srv/c.yaml"
compose_service = "bot"
"#,
        )
        .unwrap()
        .services
        .remove(0)
    }

    #[tokio::test]
    async fn update_starts_the_unit_then_answers_409_while_it_is_active() {
        let backend = FakeBackend::new(&[]);
        let client = DockerClient::new("/nonexistent");
        let (status, body) = update_reply(&backend, &client, &svc()).await.unwrap();
        assert_eq!((status, body.0), (StatusCode::ACCEPTED, json!({})));
        let (status, body) = update_reply(&backend, &client, &svc()).await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.0["error"].is_string());
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["unit chulacraft-update@discord-bot.service"]
        );
    }

    #[tokio::test]
    async fn image_with_the_proxy_down_is_503() {
        let client = DockerClient::new("/nonexistent");
        let (status, _) = image_reply(&client, &svc()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn only_updatable_docker_services_match() {
        let config = Config::parse(
            r#"
[[service]]
id = "a"
name = "A"
backend = "docker"
container = "a"
compose_file = "/x"
compose_service = "a"

[[service]]
id = "b"
name = "B"
backend = "docker"
container = "b"

[[service]]
id = "c"
name = "C"
backend = "systemd"
unit = "minecraft@c.service"
"#,
        )
        .unwrap();
        let state = AppState {
            backend: Arc::new(FakeBackend::new(&[])),
            config,
            jwt: None,
            trusted_uids: vec![],
            game_uid: None,
        };
        assert!(updatable(&state, "a").is_some());
        for id in ["b", "c", "missing"] {
            assert!(updatable(&state, id).is_none(), "{id}");
        }
    }
}
