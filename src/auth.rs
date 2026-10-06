use std::{ffi::CString, os::fd::{AsRawFd, RawFd}, sync::Arc};

use axum::{extract::{ConnectInfo, FromRequestParts, connect_info::Connected}, http::{HeaderMap, StatusCode, header, request::Parts}, serve::IncomingStream};
use tokio::net::{UnixListener, UnixStream};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

use crate::{AppState, handlers::AppError};

// Permission bits carried in the token's `permission` claim. Unknown bits are
// ignored, so the website can grant bits this build doesn't know yet.
pub const STATUS: u32 = 1;
pub const LOGS: u32 = 2;
pub const CONSOLE_READ: u32 = 4;
pub const CONSOLE_WRITE: u32 = 8;
pub const START: u32 = 16;
pub const STOP: u32 = 32;
pub const RESTART: u32 = 64;

/// Browsers can't set headers on a WebSocket, so the console sends the token as
/// the second subprotocol: `Sec-WebSocket-Protocol: mcsv.jwt, <token>`.
pub const WS_PROTOCOL: &str = "mcsv.jwt";

/// Local callers in any of these groups skip the token: they could reach the
/// units through systemd anyway. Everyone else (Apache runs as www-data, which
/// must not be in them) needs a JWT.
pub const UNIX_GROUPS: [&str; 2] = ["mcsv-mgr", "adm"];

/// The game servers run as this user, which is also in mcsv-mgr. A plugin must
/// not be able to drive the manager, so it never gets the unix shortcut.
pub const GAME_USER: &str = "mcsv";

#[derive(serde::Deserialize, Debug, Clone)]
pub struct Claims {
    pub sub: String,
    pub jti: String,
    pub permission: u32,
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
    pub fn from_env() -> Self {
        let raw = std::env::var("MCSV_JWT_PUBLIC_KEY")
            .expect("MCSV_JWT_PUBLIC_KEY is required: the manager refuses to run without auth");
        let pem = if raw.contains("-----BEGIN") {
            raw
        } else {
            format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", raw.trim())
        };
        Self::new(DecodingKey::from_ed_pem(pem.as_bytes()).expect("MCSV_JWT_PUBLIC_KEY is not an Ed25519 public key"))
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
        decode::<Claims>(token, &self.key, &self.validation).ok().map(|data| data.claims)
    }
}

/// Group ids for `UNIX_GROUPS`; groups missing on this host are skipped.
/// Call once at startup: getgrnam isn't thread-safe.
pub fn unix_gids() -> Vec<u32> {
    UNIX_GROUPS.iter().filter_map(|name| {
        let name = CString::new(*name).ok()?;
        let group = unsafe { libc::getgrnam(name.as_ptr()) };
        (!group.is_null()).then(|| unsafe { (*group).gr_gid })
    }).collect()
}

/// Uid of `GAME_USER`, if it exists. Same startup-only rule as `unix_gids`.
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
    pub groups: Vec<u32>,
}

impl Peer {
    fn of(stream: &UnixStream) -> Self {
        let cred = stream.peer_cred().ok();
        let mut groups = peer_groups(stream.as_raw_fd());
        groups.extend(cred.map(|c| c.gid()));
        Peer { uid: cred.map(|c| c.uid()), groups }
    }
}

impl Connected<IncomingStream<'_, UnixListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        Peer::of(stream.io())
    }
}

/// Supplementary groups via SO_PEERGROUPS. More than 256 groups fails closed
/// (empty list), which just means that caller needs a token.
fn peer_groups(fd: RawFd) -> Vec<u32> {
    let mut groups = vec![0 as libc::gid_t; 256];
    let mut len = (groups.len() * size_of::<libc::gid_t>()) as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERGROUPS, groups.as_mut_ptr().cast(), &mut len)
    } == 0;
    groups.truncate(if ok { len as usize / size_of::<libc::gid_t>() } else { 0 });
    groups
}

/// A peer in an allowed group gets every permission, no token needed.
fn unix_auth(peer: &Peer, gids: &[u32], game_uid: Option<u32>) -> Option<Auth> {
    let uid = peer.uid.filter(|&u| Some(u) != game_uid)?;
    peer.groups.iter().any(|g| gids.contains(g)).then(|| Auth(Claims {
        sub: format!("uid {uid}"),
        jti: "unix".into(),
        permission: u32::MAX,
    }))
}

fn token(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        return value.strip_prefix("Bearer ");
    }
    let protocols = headers.get(header::SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    let mut parts = protocols.split(',').map(str::trim);
    if parts.next()? != WS_PROTOCOL { return None; }
    parts.next()
}

/// The verified caller. Handlers take this and call `require` with the bits
/// their route needs; a missing or bad token never reaches the handler body.
pub struct Auth(pub Claims);

impl Auth {
    pub fn has(&self, bits: u32) -> bool {
        self.0.permission & bits == bits
    }

    pub fn require(&self, bits: u32) -> Result<(), AppError> {
        if self.has(bits) { Ok(()) } else { Err(AppError::Api(StatusCode::FORBIDDEN.into())) }
    }
}

impl FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, AppError> {
        // <RequireAny> unix group, JWT </RequireAny>
        if let Some(auth) = parts.extensions.get::<ConnectInfo<Peer>>().and_then(|ConnectInfo(p)| unix_auth(p, &state.unix_gids, state.game_uid)) {
            return Ok(auth);
        }
        token(&parts.headers)
            .and_then(|t| state.jwt.verify(t))
            .map(Auth)
            .ok_or(AppError::Api(StatusCode::UNAUTHORIZED.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    // Throwaway pair generated for these tests only.
    const PRIVATE: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIEdC1r034m2hJxfU5JhJTxXWgYxO4UNNB5m2wEVx1Lrw\n-----END PRIVATE KEY-----\n";
    const PUBLIC: &str = "MCowBQYDK2VwAyEA2xUoKv6AN4BdDOL3w4jdT5//wM21JOJqtKniJe6nQ7c=";

    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    fn jwt() -> Jwt {
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{PUBLIC}\n-----END PUBLIC KEY-----\n");
        Jwt::new(DecodingKey::from_ed_pem(pem.as_bytes()).unwrap())
    }

    fn sign(claims: serde_json::Value) -> String {
        encode(&Header::new(Algorithm::EdDSA), &claims, &EncodingKey::from_ed_pem(PRIVATE.as_bytes()).unwrap()).unwrap()
    }

    fn claims(exp: u64) -> serde_json::Value {
        serde_json::json!({ "iss": "chulacraft-web", "aud": "mcsv-manager", "sub": "Krisanapon", "jti": "j1", "iat": now(), "exp": exp, "permission": 71 })
    }

    #[test]
    fn accepts_a_valid_token() {
        let c = jwt().verify(&sign(claims(now() + 60))).unwrap();
        assert_eq!((c.sub.as_str(), c.permission), ("Krisanapon", 71));
        let auth = Auth(c);
        assert!(auth.has(STATUS | LOGS | CONSOLE_READ | RESTART));
        assert!(!auth.has(CONSOLE_WRITE) && !auth.has(STOP));
    }

    #[test]
    fn rejects_expired_foreign_or_tampered_tokens() {
        assert!(jwt().verify(&sign(claims(now() - 120))).is_none());
        let mut other = claims(now() + 60);
        other["aud"] = "something-else".into();
        assert!(jwt().verify(&sign(other)).is_none());
        let token = sign(claims(now() + 60));
        let (head, sig) = token.rsplit_once('.').unwrap();
        let forged = sign(serde_json::json!({ "permission": 2147483647 }));
        let forged_payload = forged.split('.').nth(1).unwrap();
        let header = head.split('.').next().unwrap();
        assert!(jwt().verify(&format!("{header}.{forged_payload}.{sig}")).is_none());
        // alg=none must never verify.
        assert!(jwt().verify(&format!("eyJhbGciOiJub25lIn0.{forged_payload}.")).is_none());
    }

    #[test]
    fn accepts_a_token_minted_by_the_website() {
        // chulacraft-web's mintToken() with the test key above, expiring in 2099.
        let token = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJjaHVsYWNyYWZ0LXdlYiIsImF1ZCI6Im1jc3YtbWFuYWdlciIsInN1YiI6IktyaXNhbmFwb24iLCJqdGkiOiJ3ZWItMSIsImlhdCI6NDA3MDkwODgwMCwiZXhwIjo0MDcwOTA4ODYwLCJwZXJtaXNzaW9uIjoyMTQ3NDgzNjQ3fQ.PEnqWquAZp_WFYf1FJM_p6TPV1yTAMy2k2w-W0n27vWb-bLCZxFUwdV4nFTolqvjs4WPDxVKBesGNZexJ3syAg";
        let c = jwt().verify(token).unwrap();
        assert_eq!((c.sub.as_str(), c.jti.as_str(), c.permission), ("Krisanapon", "web-1", 2147483647));
    }

    #[tokio::test]
    async fn unix_peer_in_an_allowed_group_skips_the_token() {
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = Peer::of(&a);
        let me = unsafe { libc::getegid() };
        assert_eq!(peer.uid, Some(unsafe { libc::geteuid() }));
        assert!(peer.groups.contains(&me));
        let auth = unix_auth(&peer, &[me], None).unwrap();
        assert!(auth.has(u32::MAX));
        assert!(unix_auth(&peer, &[u32::MAX - 1], None).is_none());
        // The game-server account never skips the token, whatever its groups.
        assert!(unix_auth(&peer, &[me], peer.uid).is_none());
    }

    #[test]
    fn reads_the_token_from_either_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(token(&headers), Some("abc"));
        let mut headers = HeaderMap::new();
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, "mcsv.jwt, abc".parse().unwrap());
        assert_eq!(token(&headers), Some("abc"));
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, "chat, abc".parse().unwrap());
        assert_eq!(token(&headers), None);
    }
}
