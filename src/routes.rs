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
            // /backups is implemented (T2.9): an empty POST body is a 400.
            let expected = match (method, path) {
                ("GET", "/backups") => StatusCode::OK,
                ("POST", "/backups") => StatusCode::BAD_REQUEST,
                _ => StatusCode::NOT_IMPLEMENTED,
            };
            assert_eq!(right, expected, "{method} {path}");
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
}
