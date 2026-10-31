use std::{ffi::CString, io, sync::Arc};

use axum::{
    extract::{ConnectInfo, FromRequestParts, connect_info::Connected},
    http::{HeaderMap, StatusCode, header, request::Parts},
    serve::IncomingStream,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use tokio::net::UnixListener;

use crate::{handlers::AppError, state::AppState};

pub type PermissionBits = u32;

// Permission bits carried in the token's `permission` claim. Unknown bits are
// ignored, so the website can grant bits this build doesn't know yet.
pub const STATUS: PermissionBits = 1;
pub const LOGS: PermissionBits = 2;
pub const CONSOLE_READ: PermissionBits = 4;
pub const CONSOLE_WRITE: PermissionBits = 8;
pub const START: PermissionBits = 16;
pub const STOP: PermissionBits = 32;
pub const RESTART: PermissionBits = 64;
pub const BACKUP_READ: PermissionBits = 128;
pub const BACKUP_RUN: PermissionBits = 256;
pub const UPDATE: PermissionBits = 512;

/// Browsers can't set headers on a WebSocket, so the console sends the token as
/// the second subprotocol: `Sec-WebSocket-Protocol: mcsv.jwt, <token>`.
pub const WS_PROTOCOL: &str = "mcsv.jwt";

/// The game servers run as this user. A plugin must not be able to drive the
/// manager, so it never gets the unix shortcut, even if its uid is listed.
pub const GAME_USER: &str = "mcsv";

#[derive(serde::Deserialize, Debug, Clone)]
pub struct Claims {
    pub sub: String,
    pub jti: String,
    pub permission: PermissionBits,
}

/// Verifies tokens minted by chulacraft-web. Only the public key lives here, so
/// this host can check tokens but never mint one.
pub struct Jwt {
    key: DecodingKey,
    validation: Validation,
}

impl std::fmt::Debug for Jwt {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Jwt")
    }
}

impl Jwt {
    /// `MCSV_JWT_PUBLIC_KEY`: an Ed25519 public key, as PEM or as the one-line
    /// base64 body of one (easier to put in a systemd EnvironmentFile).
    pub fn from_env() -> Result<Self, io::Error> {
        let raw = std::env::var("MCSV_JWT_PUBLIC_KEY").map_err(|_| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "MCSV_JWT_PUBLIC_KEY is required: the manager refuses to run without auth",
            )
        })?;
        let pem = if raw.contains("-----BEGIN") {
            raw
        } else {
            format!(
                "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                raw.trim()
            )
        };
        Ok(Self::new(
            DecodingKey::from_ed_pem(pem.as_bytes()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MCSV_JWT_PUBLIC_KEY is not an Ed25519 public key",
                )
            })?,
        ))
    }

    pub fn new(key: DecodingKey) -> Self {
        // Pinned to EdDSA: no `none`, no HS/RS fallback.
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&["chulacraft-web"]);
        validation.set_audience(&["mcsv-manager"]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 30;
        Self { key, validation }
    }

    pub fn verify(&self, token: &str) -> Option<Claims> {
        decode::<Claims>(token, &self.key, &self.validation)
            .ok()
            .map(|data| data.claims)
    }
}

/// `MCSV_TRUSTED_UIDS`: numeric uids, separated by commas or spaces. Root is
/// always trusted and needs no entry. A bad entry is an error, not a skip, so a
/// typo can't quietly leave an operator locked out (or in).
pub fn parse_uids(raw: &str) -> Result<Vec<u32>, String> {
    raw.split([',', ' ', '\n'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u32>()
                .map_err(|_| format!("MCSV_TRUSTED_UIDS: {part:?} is not a numeric uid"))
        })
        .collect()
}

pub fn trusted_uids_from_env() -> Result<Vec<u32>, String> {
    parse_uids(&std::env::var("MCSV_TRUSTED_UIDS").unwrap_or_default())
}

/// Uid of `GAME_USER`, if it exists. Call once at startup: getpwnam isn't thread-safe.
pub fn game_uid() -> Option<u32> {
    let name = CString::new(GAME_USER).ok()?;
    let user = unsafe { libc::getpwnam(name.as_ptr()) };
    (!user.is_null()).then(|| unsafe { (*user).pw_uid })
}

/// Who is on the other end of the socket, as the kernel recorded it at
/// connect(): nothing the client sends can change it.
#[derive(Clone, Debug)]
pub struct Peer {
    pub uid: Option<u32>,
}

impl Peer {
    fn of(stream: &tokio::net::UnixStream) -> Self {
        Peer {
            uid: stream.peer_cred().ok().map(|c| c.uid()),
        }
    }
}

impl Connected<IncomingStream<'_, UnixListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        Peer::of(stream.io())
    }
}

/// The unix shortcut: only root and listed uids, never a group, and never the
/// game-server account. Returns true when this peer may skip the token.
fn unix_trusted(peer_uid: Option<u32>, trusted: &[u32], game_uid: Option<u32>) -> bool {
    match peer_uid {
        Some(uid) if Some(uid) != game_uid => uid == 0 || trusted.contains(&uid),
        _ => false,
    }
}

fn token(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        return value.strip_prefix("Bearer ");
    }
    let protocols = headers.get(header::SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    let mut parts = protocols.split(',').map(str::trim);
    if parts.next()? != WS_PROTOCOL {
        return None;
    }
    parts.next()
}

/// Why a request was turned away. Logged as written here: no free text and no
/// request data, so a token can never end up in the journal.
#[derive(Debug, PartialEq, Eq)]
pub enum DenyReason {
    NoToken,
    BadToken,
    NoVerificationKey,
    MissingBits {
        have: PermissionBits,
        need: PermissionBits,
    },
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::NoToken => f.write_str("no token"),
            Self::BadToken => f.write_str("token rejected"),
            Self::NoVerificationKey => f.write_str("no verification key configured"),
            Self::MissingBits { have, need } => {
                write!(f, "missing permission bits (have {have}, need {need})")
            }
        }
    }
}

/// One journal line per 401/403: path, peer uid and reason. Never the token.
pub fn denial_line(
    status: StatusCode,
    path: &str,
    peer_uid: Option<u32>,
    reason: &DenyReason,
) -> String {
    let uid = peer_uid.map_or("-".to_string(), |u| u.to_string());
    format!(
        "auth denied: {} path={path} peer_uid={uid} reason={reason}",
        status.as_u16()
    )
}

/// `/service*` and `/backups` answer errors as `{ "error": string }`; the
/// older `/server*`, `/servers` and `/status` keep their empty bodies.
fn wants_json(path: &str) -> bool {
    path == "/services"
        || path == "/backups"
        || path.starts_with("/service/")
        || path.starts_with("/backups/")
}

/// Who the request is, once the token or the unix peer has been checked.
pub enum Identity {
    Claims(Claims),
    Bypass,
}

/// The verified caller. Handlers take this and call `require` with the bits
/// their route needs. A request with no valid identity never reaches a handler:
/// the extractor answers 401 first.
pub struct Auth {
    identity: Identity,
    path: String,
    peer_uid: Option<u32>,
}

impl Auth {
    pub fn has(&self, bits: PermissionBits) -> bool {
        (self.get_permission() & bits) == bits
    }

    /// 403 (and a journal line) when the identity lacks any of `bits`.
    pub fn require(&self, bits: PermissionBits) -> Result<(), AppError> {
        if self.has(bits) {
            return Ok(());
        }
        let reason = DenyReason::MissingBits {
            have: self.get_permission(),
            need: bits,
        };
        Err(deny(
            StatusCode::FORBIDDEN,
            &self.path,
            self.peer_uid,
            &reason,
        ))
    }

    pub fn claims(&self) -> Option<&Claims> {
        match &self.identity {
            Identity::Claims(c) => Some(c),
            Identity::Bypass => None,
        }
    }

    pub fn get_permission(&self) -> PermissionBits {
        match &self.identity {
            Identity::Claims(c) => c.permission,
            Identity::Bypass => u32::MAX,
        }
    }
}

fn deny(status: StatusCode, path: &str, peer_uid: Option<u32>, reason: &DenyReason) -> AppError {
    eprintln!("{}", denial_line(status, path, peer_uid, reason));
    AppError::Denied {
        status,
        json: wants_json(path),
    }
}

impl FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, AppError> {
        let path = parts.uri.path().to_string();
        let peer_uid = parts
            .extensions
            .get::<ConnectInfo<Peer>>()
            .and_then(|ConnectInfo(p)| p.uid);
        let auth = |identity| Auth {
            identity,
            path: path.clone(),
            peer_uid,
        };

        // A presented token decides the request, whoever the peer is: a verified
        // one wins, anything else is a 401. Only token-less requests may use the
        // trusted-unix shortcut, so a relay running as root can't launder a bad token.
        if let Some(t) = token(&parts.headers) {
            let reason = match state.jwt.as_ref() {
                None => DenyReason::NoVerificationKey,
                Some(jwt) => match jwt.verify(t) {
                    Some(claims) => return Ok(auth(Identity::Claims(claims))),
                    None => DenyReason::BadToken,
                },
            };
            return Err(deny(StatusCode::UNAUTHORIZED, &path, peer_uid, &reason));
        }
        if unix_trusted(peer_uid, &state.trusted_uids, state.game_uid) {
            return Ok(auth(Identity::Bypass));
        }
        Err(deny(
            StatusCode::UNAUTHORIZED,
            &path,
            peer_uid,
            &DenyReason::NoToken,
        ))
    }
}

/// Throwaway Ed25519 pair and token helpers, shared with the router tests.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    // Throwaway pair generated for these tests only.
    const PRIVATE: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIEdC1r034m2hJxfU5JhJTxXWgYxO4UNNB5m2wEVx1Lrw\n-----END PRIVATE KEY-----\n";
    const PUBLIC: &str = "MCowBQYDK2VwAyEA2xUoKv6AN4BdDOL3w4jdT5//wM21JOJqtKniJe6nQ7c=";

    pub fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    pub fn jwt() -> Jwt {
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{PUBLIC}\n-----END PUBLIC KEY-----\n");
        Jwt::new(DecodingKey::from_ed_pem(pem.as_bytes()).unwrap())
    }

    pub fn sign(claims: serde_json::Value) -> String {
        encode(
            &Header::new(Algorithm::EdDSA),
            &claims,
            &EncodingKey::from_ed_pem(PRIVATE.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    pub fn claims(exp: u64, permission: u32) -> serde_json::Value {
        serde_json::json!({ "iss": "chulacraft-web", "aud": "mcsv-manager", "sub": "Krisanapon", "jti": "j1", "iat": now(), "exp": exp, "permission": permission })
    }

    /// A valid token carrying exactly `permission`.
    pub fn token_with(permission: u32) -> String {
        sign(claims(now() + 60, permission))
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[test]
    fn accepts_a_valid_token() {
        let c = jwt().verify(&sign(claims(now() + 60, 71))).unwrap();
        assert_eq!((c.sub.as_str(), c.permission), ("Krisanapon", 71));
        let auth = Auth {
            identity: Identity::Claims(c),
            path: "/".into(),
            peer_uid: None,
        };
        assert!(auth.has(STATUS | LOGS | CONSOLE_READ | RESTART));
        assert!(!auth.has(CONSOLE_WRITE) && !auth.has(STOP));
    }

    #[test]
    fn rejects_expired_foreign_or_tampered_tokens() {
        assert!(jwt().verify(&sign(claims(now() - 120, 71))).is_none());
        let mut other = claims(now() + 60, 71);
        other["aud"] = "something-else".into();
        assert!(jwt().verify(&sign(other)).is_none());
        let token = sign(claims(now() + 60, 71));
        let (head, sig) = token.rsplit_once('.').unwrap();
        let forged = sign(serde_json::json!({ "permission": 2147483647 }));
        let forged_payload = forged.split('.').nth(1).unwrap();
        let header = head.split('.').next().unwrap();
        assert!(
            jwt()
                .verify(&format!("{header}.{forged_payload}.{sig}"))
                .is_none()
        );
        // alg=none must never verify.
        assert!(
            jwt()
                .verify(&format!("eyJhbGciOiJub25lIn0.{forged_payload}."))
                .is_none()
        );
    }

    #[test]
    fn accepts_a_token_minted_by_the_website() {
        // chulacraft-web's mintToken() with the test key above, expiring in 2099.
        let token = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJjaHVsYWNyYWZ0LXdlYiIsImF1ZCI6Im1jc3YtbWFuYWdlciIsInN1YiI6IktyaXNhbmFwb24iLCJqdGkiOiJ3ZWItMSIsImlhdCI6NDA3MDkwODgwMCwiZXhwIjo0MDcwOTA4ODYwLCJwZXJtaXNzaW9uIjoyMTQ3NDgzNjQ3fQ.PEnqWquAZp_WFYf1FJM_p6TPV1yTAMy2k2w-W0n27vWb-bLCZxFUwdV4nFTolqvjs4WPDxVKBesGNZexJ3syAg";
        let c = jwt().verify(token).unwrap();
        assert_eq!(
            (c.sub.as_str(), c.jti.as_str(), c.permission),
            ("Krisanapon", "web-1", 2147483647)
        );
    }

    #[tokio::test]
    async fn peer_uid_comes_from_the_kernel() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        assert_eq!(Peer::of(&a).uid, Some(unsafe { libc::geteuid() }));
    }

    #[test]
    fn unix_shortcut_is_root_and_listed_uids_only() {
        let trusted = [1001, 1002];
        assert!(unix_trusted(Some(0), &trusted, None));
        assert!(unix_trusted(Some(1001), &trusted, None));
        assert!(unix_trusted(Some(1002), &trusted, None));
        // www-data, an unlisted user, and an unknown peer all need a token.
        assert!(!unix_trusted(Some(33), &trusted, None));
        assert!(!unix_trusted(Some(1003), &trusted, None));
        assert!(!unix_trusted(None, &trusted, None));
    }

    #[test]
    fn the_game_account_never_skips_the_token_even_if_listed() {
        assert!(!unix_trusted(Some(998), &[998], Some(998)));
        assert!(unix_trusted(Some(1001), &[998, 1001], Some(998)));
    }

    #[test]
    fn trusted_uid_list_parsing() {
        assert_eq!(parse_uids("").unwrap(), Vec::<u32>::new());
        assert_eq!(
            parse_uids("1000, 1001 1002\n").unwrap(),
            vec![1000, 1001, 1002]
        );
        assert!(parse_uids("1000,kai").unwrap_err().contains("kai"));
        assert!(parse_uids("-1").is_err());
        // A group name or anything else non-numeric is refused rather than skipped.
        assert!(parse_uids("adm").is_err());
    }

    #[test]
    fn reads_the_token_from_either_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(token(&headers), Some("abc"));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "mcsv.jwt, abc".parse().unwrap(),
        );
        assert_eq!(token(&headers), Some("abc"));
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, "chat, abc".parse().unwrap());
        assert_eq!(token(&headers), None);
    }

    #[test]
    fn json_errors_only_for_the_new_api() {
        for p in ["/services", "/service/a/status", "/backups", "/backups/x"] {
            assert!(wants_json(p), "{p}");
        }
        for p in ["/servers", "/server/a/status", "/status", "/servicesx"] {
            assert!(!wants_json(p), "{p}");
        }
    }

    #[test]
    fn denial_lines_name_path_uid_and_reason_only() {
        let line = denial_line(
            StatusCode::UNAUTHORIZED,
            "/services",
            Some(33),
            &DenyReason::BadToken,
        );
        assert_eq!(
            line,
            "auth denied: 401 path=/services peer_uid=33 reason=token rejected"
        );
        let line = denial_line(
            StatusCode::FORBIDDEN,
            "/server/a/cmd",
            None,
            &DenyReason::MissingBits { have: 1, need: 8 },
        );
        assert_eq!(
            line,
            "auth denied: 403 path=/server/a/cmd peer_uid=- reason=missing permission bits (have 1, need 8)"
        );
    }
}
