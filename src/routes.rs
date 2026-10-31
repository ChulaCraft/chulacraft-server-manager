use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::{api, handlers, state::AppState};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", get(handlers::get_host_status))
        .route("/servers", get(handlers::list_servers))
        .route("/server/{id}/status", get(handlers::get_server_status))
        .route("/server/{id}/console", get(handlers::handle_server_console))
        .route("/server/{id}/log", get(handlers::get_server_log))
        .route("/server/{id}/rlog", get(handlers::get_server_rlog))
        // POST, so a link or prefetch can never fire them.
        .route("/server/{id}/cmd", post(handlers::handle_server_command))
        .route(
            "/server/{id}/{action}",
            post(handlers::handle_server_action),
        )
        .merge(api::routes())
        .with_state(state)
}

/// Auth matrix and routing, through the real router with a `FakeBackend`.
#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{
        auth::{self, Peer, testkit},
        backend::FakeBackend,
        config::Config,
    };

    const WWW_DATA: u32 = 33;
    const OPERATOR: u32 = 1001;
    const GAME: u32 = 998;

    fn state() -> (Arc<AppState>, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::new(&["survival"]));
        let state = Arc::new(AppState {
            backend: backend.clone(),
            config: Config::default(),
            jwt: Some(testkit::jwt()),
            trusted_uids: vec![OPERATOR],
            game_uid: Some(GAME),
        });
        (state, backend)
    }

    struct Call {
        method: &'static str,
        path: &'static str,
        bearer: Option<String>,
        peer: Option<u32>,
        body: Option<&'static str>,
    }

    fn get(path: &'static str) -> Call {
        Call {
            method: "GET",
            path,
            bearer: None,
            peer: None,
            body: None,
        }
    }

    fn post(path: &'static str) -> Call {
        Call {
            method: "POST",
            path,
            bearer: None,
            peer: None,
            body: None,
        }
    }

    impl Call {
        fn token(mut self, permission: u32) -> Self {
            self.bearer = Some(testkit::token_with(permission));
            self
        }
        fn bearer(mut self, token: &str) -> Self {
            self.bearer = Some(token.to_string());
            self
        }
        fn peer(mut self, uid: u32) -> Self {
            self.peer = Some(uid);
            self
        }
        fn body(mut self, body: &'static str) -> Self {
            self.body = Some(body);
            self
        }
    }

    async fn send(
        state: &Arc<AppState>,
        call: Call,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut req = Request::builder().method(call.method).uri(call.path);
        if let Some(t) = &call.bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if call.body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let mut req = req.body(Body::from(call.body.unwrap_or(""))).unwrap();
        if let Some(uid) = call.peer {
            req.extensions_mut()
                .insert(ConnectInfo(Peer { uid: Some(uid) }));
        }
        let res = router(state.clone()).oneshot(req).await.unwrap();
        let (parts, body) = res.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn err_body(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn no_token_is_401_not_403() {
        let (state, _) = state();
        // TCP-like (no peer) and a www-data unix peer behave the same.
        for peer in [None, Some(WWW_DATA)] {
            let mut call = get("/servers");
            call.peer = peer;
            let (status, headers, body) = send(&state, call).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{peer:?}");
            assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
            assert_eq!(body, "", "the old routes keep their empty bodies");
        }
    }

    #[tokio::test]
    async fn bad_expired_and_foreign_tokens_are_401() {
        let (state, _) = state();
        let expired = testkit::sign(testkit::claims(testkit::now() - 120, 1));
        let mut foreign = testkit::claims(testkit::now() + 60, 1);
        foreign["aud"] = "other".into();
        for token in ["garbage".to_string(), expired, testkit::sign(foreign)] {
            let (status, _, _) = send(&state, get("/servers").bearer(&token)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn a_missing_verification_key_is_401() {
        let (state, _) = state();
        let state = Arc::new(AppState {
            backend: state.backend.clone(),
            config: Config::default(),
            jwt: None,
            trusted_uids: vec![],
            game_uid: None,
        });
        let (status, _, _) = send(&state, get("/servers").token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_token_without_the_bit_is_403_and_with_it_is_200() {
        let (state, _) = state();
        let (status, _, body) = send(&state, get("/servers").token(auth::LOGS)).await;
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, ""));
        let (status, _, body) = send(&state, get("/servers").token(auth::STATUS)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, r#"["survival"]"#));
        // Unknown bits are ignored.
        let (status, _, _) = send(&state, get("/servers").token(auth::STATUS | (1 << 20))).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn www_data_with_a_valid_token_is_200_and_a_token_beats_the_peer() {
        let (state, _) = state();
        let (status, _, _) = send(&state, get("/servers").token(auth::STATUS).peer(WWW_DATA)).await;
        assert_eq!(status, StatusCode::OK);
        // Root with a token that lacks the bit: the verified token wins, so 403.
        let (status, _, _) = send(&state, get("/servers").token(auth::LOGS).peer(0)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn unix_shortcut_is_root_and_listed_uids_only() {
        let (state, backend) = state();
        for (uid, want) in [
            (0, StatusCode::OK),
            (OPERATOR, StatusCode::OK),
            (WWW_DATA, StatusCode::UNAUTHORIZED),
            (1002, StatusCode::UNAUTHORIZED),
            (GAME, StatusCode::UNAUTHORIZED),
        ] {
            let (status, _, _) = send(&state, post("/server/survival/restart").peer(uid)).await;
            assert_eq!(status, want, "uid {uid}");
        }
        assert_eq!(*backend.calls.lock().unwrap(), vec!["restart survival"; 2]);
    }

    #[tokio::test]
    async fn a_presented_bad_token_is_401_even_from_a_trusted_peer() {
        let (state, backend) = state();
        let expired = testkit::sign(testkit::claims(testkit::now() - 120, 1));
        let mut foreign = testkit::claims(testkit::now() + 60, 1);
        foreign["aud"] = "other".into();
        let keyless = Arc::new(AppState {
            backend: backend.clone(),
            config: Config::default(),
            jwt: None,
            trusted_uids: vec![OPERATOR],
            game_uid: Some(GAME),
        });
        for token in ["garbage".to_string(), expired, testkit::sign(foreign)] {
            for uid in [0, OPERATOR] {
                let call = post("/server/survival/restart").bearer(&token).peer(uid);
                let (status, _, _) = send(&state, call).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "uid {uid}");
                // No verification key: a presented token still can't fall through.
                let call = post("/server/survival/restart").bearer(&token).peer(uid);
                let (status, _, _) = send(&keyless, call).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "keyless uid {uid}");
            }
        }
        assert!(backend.calls.lock().unwrap().is_empty());
        // No token at all from root is still the shortcut.
        let (status, _, _) = send(&state, post("/server/survival/restart").peer(0)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_listed_game_account_still_needs_a_token() {
        let (state, _) = state();
        let state = Arc::new(AppState {
            backend: state.backend.clone(),
            config: Config::default(),
            jwt: Some(testkit::jwt()),
            trusted_uids: vec![GAME],
            game_uid: Some(GAME),
        });
        let (status, _, _) = send(&state, get("/servers").peer(GAME)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn new_routes_answer_errors_as_json() {
        let (state, _) = state();
        for path in ["/services", "/service/survival/status", "/backups"] {
            let (status, headers, body) = send(&state, get(path)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(
                err_body(&body),
                serde_json::json!({ "error": "unauthorized" })
            );
            assert_eq!(headers[header::CONTENT_TYPE], "application/json");
            let (status, _, body) = send(&state, get(path).token(1 << 15)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
            assert_eq!(err_body(&body), serde_json::json!({ "error": "forbidden" }));
        }
        let (status, _, body) = send(&state, post("/backups").token(auth::BACKUP_READ)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(err_body(&body)["error"], "forbidden");
    }

    #[tokio::test]
    async fn every_new_route_needs_its_own_bit() {
        let (state, _) = state();
        for (call, bit) in [
            (get("/services"), auth::STATUS),
            (get("/service/a/status"), auth::STATUS),
            (get("/service/a/metrics"), auth::STATUS),
            (get("/service/a/uptime"), auth::STATUS),
            (get("/service/a/rlog"), auth::LOGS),
            (post("/service/a/start"), auth::START),
            (post("/service/a/stop"), auth::STOP),
            (post("/service/a/restart"), auth::RESTART),
            (get("/service/a/image"), auth::STATUS),
            (post("/service/a/update"), auth::UPDATE),
            (get("/backups"), auth::BACKUP_READ),
            (post("/backups"), auth::BACKUP_RUN),
        ] {
            let (method, path) = (call.method, call.path);
            let wrong = send(
                &state,
                Call {
                    bearer: None,
                    ..call
                }
                .token(!bit & 0x3ff),
            )
            .await
            .0;
            assert_eq!(
                wrong,
                StatusCode::FORBIDDEN,
                "{method} {path} with every other bit"
            );
            let call = if method == "GET" {
                get(path)
            } else {
                post(path)
            };
            let right = send(&state, call.token(bit)).await.0;
            // The service API (T2.6) answers 404 for the unknown service `a`;
            // image, update and backups are still stubs.
            let want =
                if path.ends_with("/image") || path.ends_with("/update") || path == "/backups" {
                    StatusCode::NOT_IMPLEMENTED
                } else if path == "/services" {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                };
            assert_eq!(right, want, "{method} {path}");
        }
        let (status, _, body) = send(&state, post("/service/a/explode").token(0x3ff)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(err_body(&body)["error"], "not found");
    }

    #[tokio::test]
    async fn old_routes_keep_working_with_the_right_bits() {
        let (state, backend) = state();
        let (status, _, body) =
            send(&state, get("/server/survival/status").token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""active_state":"active""#), "{body}");
        let (status, _, body) = send(
            &state,
            get("/server/survival/rlog?until=9999").token(auth::LOGS),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<Steve> hi"), "{body}");
        let (status, _, body) = send(
            &state,
            get("/server/survival/log?since=0").token(auth::LOGS),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Done"), "{body}");
        let (status, _, _) = send(&state, get("/server/nope/log?since=0").token(auth::LOGS)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &state,
            post("/server/survival/cmd")
                .token(auth::CONSOLE_WRITE)
                .body(r#"{"command":"say hi"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&state, post("/server/survival/stop").token(auth::STOP)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&state, post("/server/survival/start").token(auth::START)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&state, post("/server/survival/bogus").token(u32::MAX)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, body) = send(&state, get("/status").token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("MemTotal"));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["cmd survival say hi", "stop survival", "start survival"]
        );
    }

    /// A real handshake over TCP: the token rides in `Sec-WebSocket-Protocol`.
    #[tokio::test]
    async fn the_console_takes_its_token_from_the_subprotocol() {
        use tokio_tungstenite::{
            connect_async,
            tungstenite::{Error, client::IntoClientRequest},
        };

        let (state, _) = state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

        let connect = |protocols: Option<String>| async move {
            let mut req = format!("ws://{addr}/server/survival/console")
                .into_client_request()
                .unwrap();
            if let Some(p) = protocols {
                req.headers_mut()
                    .insert(header::SEC_WEBSOCKET_PROTOCOL, p.parse().unwrap());
            }
            connect_async(req).await
        };
        let refused = |r: Result<_, Error>| match r {
            Err(Error::Http(res)) => res.status().as_u16(),
            other => panic!("expected an HTTP refusal, got {:?}", other.map(|_| ())),
        };

        assert_eq!(refused(connect(None).await), 401);
        let weak = testkit::token_with(auth::STATUS);
        assert_eq!(
            refused(connect(Some(format!("mcsv.jwt, {weak}"))).await),
            403
        );
        assert_eq!(refused(connect(Some("chat, x".into())).await), 401);

        let token = testkit::token_with(auth::CONSOLE_READ);
        let (_, res) = connect(Some(format!("mcsv.jwt, {token}"))).await.unwrap();
        assert_eq!(res.status(), 101);
        // The browser drops the socket unless the first subprotocol is echoed back.
        assert_eq!(res.headers()[header::SEC_WEBSOCKET_PROTOCOL], "mcsv.jwt");
    }

    const SERVICES: &str = "
[[service]]
id = 't26-survival'
name = 'Survival'
backend = 'systemd'
unit = 'minecraft@survival.service'

[[service]]
id = 't26-limited'
name = 'Limited'
backend = 'systemd'
unit = 'minecraft@creative.service'
actions = ['restart']
console = false

[[service]]
id = 't26-bot'
name = 'Bot'
backend = 'docker'
container = 'c'

[backups]
world_instances = ['survival']
";

    fn svc_state() -> (Arc<AppState>, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::new(&["survival", "creative"]));
        let state = Arc::new(AppState {
            backend: backend.clone(),
            config: Config::parse(SERVICES).unwrap(),
            jwt: Some(testkit::jwt()),
            trusted_uids: vec![],
            game_uid: None,
        });
        (state, backend)
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn services_lists_the_config_in_the_contract_shape() {
        let (state, _) = svc_state();
        let (status, _, body) = send(&state, get("/services").token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json(&body),
            serde_json::json!([
                { "id": "t26-survival", "name": "Survival", "kind": "systemd", "console": true,
                  "actions": ["start", "stop", "restart"], "backups": true, "updatable": false },
                { "id": "t26-limited", "name": "Limited", "kind": "systemd", "console": false,
                  "actions": ["restart"], "backups": false, "updatable": false },
                { "id": "t26-bot", "name": "Bot", "kind": "docker", "console": false,
                  "actions": ["start", "stop", "restart"], "backups": false, "updatable": false },
            ])
        );
    }

    #[tokio::test]
    async fn status_reports_state_and_isolates_a_failing_service() {
        let (state, backend) = svc_state();
        let path = "/service/t26-survival/status";
        let (status, _, body) = send(&state, get(path).token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::OK);
        let v = json(&body);
        assert_eq!(v["state"], "up");
        assert_eq!(v["last_restart"], serde_json::Value::Null);
        assert_eq!(
            (
                v["cpu_pct"].as_f64(),
                v["mem_bytes"].as_u64(),
                v["mem_limit_bytes"].as_u64()
            ),
            (Some(0.0), Some(0), Some(0))
        );
        assert!(chrono::DateTime::parse_from_rfc3339(v["since"].as_str().unwrap()).is_ok());

        backend.set_state("survival", "inactive");
        let (_, _, body) = send(&state, get(path).token(auth::STATUS)).await;
        assert_eq!(json(&body)["state"], "down");
        backend.set_state("survival", "activating");
        let (_, _, body) = send(&state, get(path).token(auth::STATUS)).await;
        assert_eq!(json(&body)["state"], "restarting");

        backend.fail_status("creative");
        let (status, _, body) = send(
            &state,
            get("/service/t26-limited/status").token(auth::STATUS),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(json(&body)["error"].is_string());
        let (status, _, _) = send(&state, get(path).token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::OK, "another service is unaffected");

        let (status, _, body) =
            send(&state, get("/service/t26-bot/status").token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(json(&body)["error"].is_string());
        let (status, _, body) = send(&state, get("/service/nope/status").token(auth::STATUS)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&body)["error"], "unknown service");
    }

    #[tokio::test]
    async fn metrics_and_uptime_serve_stored_samples_and_check_their_ranges() {
        let (state, _) = svc_state();
        let store = crate::metrics::store();
        let now = chrono::Utc::now().timestamp();
        store.record(
            "t26-survival",
            crate::metrics::Sample {
                t: crate::metrics::bucket(now),
                cpu_pct: 8.5,
                mem_bytes: 99,
                up: true,
            },
        );
        let (status, _, body) = send(
            &state,
            get("/service/t26-survival/metrics?hours=1").token(auth::STATUS),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v = json(&body);
        assert_eq!(
            (v[0]["cpu_pct"].as_f64(), v[0]["mem_bytes"].as_u64()),
            (Some(8.5), Some(99))
        );
        assert!(v[0]["t"].as_str().unwrap().ends_with('Z'));
        let (_, _, body) = send(
            &state,
            get("/service/t26-survival/uptime").token(auth::STATUS),
        )
        .await;
        assert_eq!(json(&body)[0]["up_ratio"].as_f64(), Some(1.0));
        for bad in [
            "metrics?hours=0",
            "metrics?hours=169",
            "metrics?hours=x",
            "uptime?days=8",
            "uptime?days=0",
        ] {
            let path = format!("/service/t26-survival/{bad}");
            let path: &'static str = Box::leak(path.into_boxed_str());
            let (status, _, body) = send(&state, get(path).token(auth::STATUS)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json(&body)["error"].is_string());
        }
        let (_, _, body) = send(
            &state,
            get("/service/t26-limited/metrics").token(auth::STATUS),
        )
        .await;
        assert_eq!(json(&body), serde_json::json!([]));
    }

    #[tokio::test]
    async fn rlog_works_without_until_and_honours_it() {
        let (state, _) = svc_state();
        let (status, _, body) = send(
            &state,
            get("/service/t26-survival/rlog?max_lines=200").token(auth::LOGS),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v = json(&body);
        assert_eq!(v[0]["message"], "<Steve> hi", "newest first");
        assert_eq!(v[1]["message"], "Done (1.0s)!");
        // Fake lines are at 1000 and 2000 microseconds after the epoch.
        let (_, _, body) = send(
            &state,
            get("/service/t26-survival/rlog?until=1970-01-01T00:00:00.001500Z").token(auth::LOGS),
        )
        .await;
        let v = json(&body);
        assert_eq!(
            (v.as_array().unwrap().len(), v[0]["message"].as_str()),
            (1, Some("Done (1.0s)!"))
        );
        let (_, _, body) = send(
            &state,
            get("/service/t26-survival/rlog?max_lines=1").token(auth::LOGS),
        )
        .await;
        assert_eq!(json(&body).as_array().unwrap().len(), 1);
        for bad in ["until=yesterday", "max_lines=0", "max_lines=abc"] {
            let path: &'static str =
                Box::leak(format!("/service/t26-survival/rlog?{bad}").into_boxed_str());
            let (status, _, body) = send(&state, get(path).token(auth::LOGS)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json(&body)["error"].is_string());
        }
    }

    #[tokio::test]
    async fn actions_answer_202_404_and_409() {
        let (state, backend) = svc_state();
        for action in ["start", "stop", "restart"] {
            let path: &'static str =
                Box::leak(format!("/service/t26-survival/{action}").into_boxed_str());
            let bit = match action {
                "start" => auth::START,
                "stop" => auth::STOP,
                _ => auth::RESTART,
            };
            let (status, _, body) = send(&state, post(path).token(bit)).await;
            assert_eq!(
                (status, json(&body)),
                (StatusCode::ACCEPTED, serde_json::json!({})),
                "{action}"
            );
        }
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["start survival", "stop survival", "restart survival"]
        );
        let (_, _, body) = send(
            &state,
            get("/service/t26-survival/status").token(auth::STATUS),
        )
        .await;
        assert!(
            json(&body)["last_restart"].is_string(),
            "a restart request is remembered"
        );

        // Not in the service's `actions`, unknown service, docker (T2.7), unknown action.
        for (path, bit, want) in [
            (
                "/service/t26-limited/start",
                auth::START,
                StatusCode::NOT_FOUND,
            ),
            ("/service/nope/start", auth::START, StatusCode::NOT_FOUND),
            (
                "/service/t26-bot/start",
                auth::START,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let (status, _, body) = send(&state, post(path).token(bit)).await;
            assert_eq!(status, want, "{path}");
            assert!(json(&body)["error"].is_string());
        }
        let (status, _, _) = send(
            &state,
            post("/service/t26-limited/restart").token(auth::RESTART),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);

        // Already changing state: 409, and nothing is sent to the backend.
        let before = backend.calls.lock().unwrap().len();
        for s in ["activating", "deactivating"] {
            backend.set_state("survival", s);
            let (status, _, body) =
                send(&state, post("/service/t26-survival/stop").token(auth::STOP)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{s}");
            assert!(json(&body)["error"].is_string());
        }
        assert_eq!(backend.calls.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn the_service_console_needs_a_console_and_takes_the_subprotocol_token() {
        use tokio_tungstenite::{
            connect_async,
            tungstenite::{Error, client::IntoClientRequest},
        };
        let (state, _) = svc_state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        let connect = |id: &'static str, token: Option<String>| async move {
            let mut req = format!("ws://{addr}/service/{id}/console")
                .into_client_request()
                .unwrap();
            if let Some(t) = token {
                req.headers_mut().insert(
                    header::SEC_WEBSOCKET_PROTOCOL,
                    format!("mcsv.jwt, {t}").parse().unwrap(),
                );
            }
            connect_async(req).await
        };
        let code = |r: Result<_, Error>| match r {
            Err(Error::Http(res)) => res.status().as_u16(),
            other => panic!("expected an HTTP refusal, got {:?}", other.map(|_| ())),
        };
        let read = || Some(testkit::token_with(auth::CONSOLE_READ));
        assert_eq!(code(connect("t26-survival", None).await), 401);
        assert_eq!(
            code(connect("t26-survival", Some(testkit::token_with(auth::STATUS))).await),
            403
        );
        assert_eq!(code(connect("t26-limited", read()).await), 404);
        assert_eq!(code(connect("nope", read()).await), 404);
        let (_, res) = connect("t26-survival", read()).await.unwrap();
        assert_eq!(res.status(), 101);
        assert_eq!(res.headers()[header::SEC_WEBSOCKET_PROTOCOL], "mcsv.jwt");
    }
}
