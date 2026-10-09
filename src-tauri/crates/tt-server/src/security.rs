use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::Engine;
use http::{HeaderMap, StatusCode};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{error::CommandError, host::PageState};

const COOKIE: &str = "tt_browser";
const MAX_SESSIONS: usize = 64;
/// A page must open its event stream promptly after creating its session.
const UNCONNECTED_SESSION_LIFETIME: Duration = Duration::from_secs(120);
/// How long a page whose event stream dropped (backgrounded tab, network change) may take to
/// resume it before the page is closed and its resources released.
const DETACHED_SESSION_LIFETIME: Duration = Duration::from_secs(10 * 60);

pub struct Security {
    credential_digest: Vec<u8>,
    pub public_origin: String,
    authority: String,
    secure_cookie: bool,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

pub struct Session {
    pub id: String,
    pub csrf: String,
    pub browser: String,
    pub page: PageState,
    streams: Mutex<StreamState>,
}

struct StreamState {
    /// Open event streams of this page; normally one, briefly two while a reconnect replaces
    /// the dropped connection.
    attached: usize,
    ever_attached: bool,
    /// When the page last had no stream: its creation, or its last stream ending.
    detached_since: Instant,
}

impl StreamState {
    /// `now` is a value so grace boundaries can be checked without wall-clock waits.
    fn expired_at(&self, now: Instant) -> bool {
        let lifetime = if self.ever_attached {
            DETACHED_SESSION_LIFETIME
        } else {
            UNCONNECTED_SESSION_LIFETIME
        };
        self.attached == 0 && now.duration_since(self.detached_since) >= lifetime
    }
}

/// Created only by the registry's atomic stream claim. It stays alive with the SSE body.
/// Dropping a connection starts the grace period, not page cleanup.
pub struct StreamGuard {
    session: Arc<Session>,
}

impl StreamGuard {
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut state = self
            .session
            .streams
            .lock()
            .expect("session stream state poisoned");
        state.attached -= 1;
        if state.attached == 0 {
            state.detached_since = Instant::now();
        }
    }
}

pub enum StreamClaim {
    Attached(StreamGuard),
    /// A missing session (including a server restart), or an owned session whose grace ended.
    /// Removed sessions are returned so the host can finish asynchronous resource cleanup.
    Expired(Option<Arc<Session>>),
}

fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (l, r)| difference | (l ^ r))
        == 0
}

impl Security {
    pub fn new(username: &str, password: &str, origin: &url::Url) -> Result<Self, CommandError> {
        if password.len() < 20 || username.contains(':') || username.is_empty() {
            return Err(CommandError::BadRequest(
                "HTTP credentials need a username and a password of at least 20 bytes".into(),
            ));
        }
        if !matches!(origin.scheme(), "http" | "https")
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.path() != "/"
            || origin.host_str().is_none()
        {
            return Err(CommandError::BadRequest(
                "public-origin must be an HTTP(S) origin, without a path or credentials".into(),
            ));
        }
        let authority = &origin[url::Position::BeforeHost..url::Position::AfterPort];
        Ok(Self {
            credential_digest: Sha256::digest(format!("{username}:{password}").as_bytes()).to_vec(),
            public_origin: origin.origin().ascii_serialization(),
            authority: authority.to_string(),
            secure_cookie: origin.scheme() == "https",
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Do not trust forwarded host/origin headers; the deployment sets one exact origin.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        if headers.get("host").and_then(|v| v.to_str().ok()) != Some(&self.authority) {
            return Err(StatusCode::MISDIRECTED_REQUEST);
        }
        let authorization = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok());
        match authorization {
            Some(value)
                if constant_time_equal(&Sha256::digest(&value), &self.credential_digest) =>
            {
                Ok(())
            }
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }

    pub fn same_origin(&self, headers: &HeaderMap) -> Result<(), CommandError> {
        if let Some(origin) = headers.get("origin")
            && origin.to_str().ok() != Some(self.public_origin.as_str())
        {
            return Err(CommandError::Unauthorized(
                "Cross-origin requests are not allowed".into(),
            ));
        }
        if headers
            .get("sec-fetch-site")
            .is_some_and(|v| v != "same-origin" && v != "none")
        {
            return Err(CommandError::Unauthorized(
                "Cross-site requests are not allowed".into(),
            ));
        }
        Ok(())
    }

    pub fn create_session(
        &self,
        headers: &HeaderMap,
    ) -> Result<(Arc<Session>, Option<String>), CommandError> {
        self.same_origin(headers)?;
        let existing = browser_cookie(headers);
        let browser = existing.clone().unwrap_or_else(random_token);
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if sessions.len() >= MAX_SESSIONS {
            return Err(CommandError::TooManyRequests(
                "Too many browser sessions; close unused tabs".into(),
            ));
        }
        let session = Arc::new(Session {
            id: random_token(),
            csrf: random_token(),
            browser: browser.clone(),
            page: PageState::default(),
            streams: Mutex::new(StreamState {
                attached: 0,
                ever_attached: false,
                detached_since: Instant::now(),
            }),
        });
        sessions.insert(session.id.clone(), session.clone());
        let cookie = existing.is_none().then(|| {
            format!(
                "{COOKIE}={browser}; Path=/; HttpOnly; SameSite=Strict{}",
                if self.secure_cookie { "; Secure" } else { "" }
            )
        });
        Ok((session, cookie))
    }

    pub fn session(
        &self,
        headers: &HeaderMap,
        id: &str,
        require_csrf: bool,
    ) -> Result<Arc<Session>, CommandError> {
        self.same_origin(headers)?;
        let sessions = self.sessions.lock().expect("session registry poisoned");
        let session = sessions.get(id).ok_or_else(|| {
            CommandError::Unauthorized("Browser session expired; reload the page".into())
        })?;
        authorize_session(headers, session, require_csrf)?;
        let state = session
            .streams
            .lock()
            .expect("session stream state poisoned");
        if state.expired_at(Instant::now()) || session.page.bus.closed().is_cancelled() {
            return Err(CommandError::Unauthorized(
                "Browser session expired; reload the page".into(),
            ));
        }
        Ok(session.clone())
    }

    /// Ownership, grace and the attached count are one registry operation. The fixed lock order
    /// is registry -> streams: expiry cannot remove a session between lookup and attaching it.
    pub fn claim_stream(&self, headers: &HeaderMap, id: &str) -> Result<StreamClaim, CommandError> {
        self.claim_stream_at(headers, id, None)
    }

    fn claim_stream_at(
        &self,
        headers: &HeaderMap,
        id: &str,
        now: Option<Instant>,
    ) -> Result<StreamClaim, CommandError> {
        self.same_origin(headers)?;
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        let Some(session) = sessions.get(id).cloned() else {
            if browser_cookie(headers).is_none() {
                return Err(CommandError::Unauthorized("Missing browser cookie".into()));
            }
            return Ok(StreamClaim::Expired(None));
        };
        // Reject another browser even when this session's grace has already elapsed.
        authorize_session(headers, &session, false)?;
        let mut state = session
            .streams
            .lock()
            .expect("session stream state poisoned");
        if state.expired_at(now.unwrap_or_else(Instant::now)) {
            sessions.remove(id);
            session.page.bus.close();
            drop(state);
            return Ok(StreamClaim::Expired(Some(session)));
        }
        state.attached += 1;
        state.ever_attached = true;
        drop(state);
        Ok(StreamClaim::Attached(StreamGuard { session }))
    }

    pub fn remove_session(&self, id: &str) -> Option<Arc<Session>> {
        let session = self
            .sessions
            .lock()
            .expect("session registry poisoned")
            .remove(id)?;
        session.page.bus.close();
        Some(session)
    }

    pub fn expired_sessions(&self) -> Vec<Arc<Session>> {
        self.expired_sessions_at(None)
    }

    fn expired_sessions_at(&self, now: Option<Instant>) -> Vec<Arc<Session>> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        let ids: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| {
                let state = session
                    .streams
                    .lock()
                    .expect("session stream state poisoned");
                state.expired_at(now.unwrap_or_else(Instant::now))
                    || session.page.bus.closed().is_cancelled()
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let session = sessions.remove(&id)?;
                session.page.bus.close();
                Some(session)
            })
            .collect()
    }
}

fn authorize_session(
    headers: &HeaderMap,
    session: &Session,
    require_csrf: bool,
) -> Result<(), CommandError> {
    if browser_cookie(headers).as_deref() != Some(session.browser.as_str()) {
        return Err(CommandError::Unauthorized(
            "Session does not belong to this browser".into(),
        ));
    }
    if require_csrf {
        let csrf = headers
            .get("x-tt-csrf")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !constant_time_equal(csrf.as_bytes(), session.csrf.as_bytes()) {
            return Err(CommandError::Unauthorized("Invalid CSRF token".into()));
        }
    }
    Ok(())
}

/// The browser identity cookie, validated; downloads are bound to it like page sessions.
pub fn browser_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == COOKIE && value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit()))
                .then(|| value.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn security() -> Security {
        Security::new(
            "tt",
            "a-long-password-for-tests-only",
            &url::Url::parse("https://tt.example").unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn browser_session_requires_owner_cookie_csrf_and_exact_origin() {
        let security = security();
        let (session, cookie) = security.create_session(&HeaderMap::new()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            cookie.unwrap().split(';').next().unwrap().parse().unwrap(),
        );
        assert!(security.session(&headers, &session.id, true).is_err());
        headers.insert("x-tt-csrf", session.csrf.parse().unwrap());
        assert!(security.session(&headers, &session.id, true).is_ok());
        headers.insert("origin", "https://evil.example".parse().unwrap());
        assert!(security.session(&headers, &session.id, true).is_err());
        headers.remove("origin");
        headers.remove("cookie");
        assert!(security.session(&headers, &session.id, false).is_err());
    }
    fn owner_headers(session: &Session) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("{COOKIE}={}", session.browser).parse().unwrap(),
        );
        headers
    }

    fn attached(claim: StreamClaim) -> StreamGuard {
        match claim {
            StreamClaim::Attached(guard) => guard,
            StreamClaim::Expired(_) => panic!("expected a live session"),
        }
    }

    #[test]
    fn stream_claim_preserves_owner_and_origin_checks_even_for_expired_sessions() {
        let security = security();
        let (session, _) = security.create_session(&HeaderMap::new()).unwrap();
        let owner = owner_headers(&session);
        let created = session.streams.lock().unwrap().detached_since;
        let expired = created + UNCONNECTED_SESSION_LIFETIME;
        let mut wrong_cookie = owner.clone();
        wrong_cookie.insert(
            "cookie",
            format!("{COOKIE}={}", "0".repeat(64)).parse().unwrap(),
        );
        assert!(matches!(
            security.claim_stream_at(&wrong_cookie, &session.id, Some(expired)),
            Err(CommandError::Unauthorized(_))
        ));
        let mut wrong_origin = owner.clone();
        wrong_origin.insert("origin", "https://evil.example".parse().unwrap());
        assert!(matches!(
            security.claim_stream_at(&wrong_origin, &session.id, Some(expired)),
            Err(CommandError::Unauthorized(_))
        ));
        let claim = security
            .claim_stream_at(&owner, &session.id, Some(expired))
            .unwrap();
        assert!(matches!(claim, StreamClaim::Expired(Some(ref old)) if Arc::ptr_eq(old, &session)));
        assert!(session.page.bus.closed().is_cancelled());
        assert!(matches!(
            security.claim_stream(&owner, &session.id).unwrap(),
            StreamClaim::Expired(None)
        ));
        assert!(
            security
                .claim_stream(&HeaderMap::new(), &session.id)
                .is_err()
        );
    }

    #[test]
    fn stream_claim_and_expiry_cannot_revive_or_remove_an_attached_session() {
        let security = security();
        let (session, _) = security.create_session(&HeaderMap::new()).unwrap();
        let owner = owner_headers(&session);
        let created = session.streams.lock().unwrap().detached_since;
        let first = attached(
            security
                .claim_stream_at(&owner, &session.id, Some(created))
                .unwrap(),
        );
        let second = attached(
            security
                .claim_stream_at(&owner, &session.id, Some(created))
                .unwrap(),
        );
        // Claim wins the registry ordering: even a later expiry sweep cannot remove a live body.
        assert!(
            security
                .expired_sessions_at(Some(created + DETACHED_SESSION_LIFETIME))
                .is_empty()
        );
        drop(first);
        assert!(
            security
                .expired_sessions_at(Some(created + DETACHED_SESSION_LIFETIME))
                .is_empty()
        );
        drop(second);
        let detached = session.streams.lock().unwrap().detached_since;
        assert!(
            security
                .expired_sessions_at(Some(
                    detached + DETACHED_SESSION_LIFETIME - Duration::from_nanos(1)
                ))
                .is_empty()
        );
        let resumed = attached(
            security
                .claim_stream_at(
                    &owner,
                    &session.id,
                    Some(detached + DETACHED_SESSION_LIFETIME - Duration::from_nanos(1)),
                )
                .unwrap(),
        );
        assert!(Arc::ptr_eq(&resumed.session, &session));
        drop(resumed);
        let detached = session.streams.lock().unwrap().detached_since;
        // Expiry wins the registry ordering: a later claim cannot revive the removed page.
        let expired = security.expired_sessions_at(Some(detached + DETACHED_SESSION_LIFETIME));
        assert_eq!(expired.len(), 1);
        assert!(Arc::ptr_eq(&expired[0], &session));
        assert!(matches!(
            security.claim_stream(&owner, &session.id).unwrap(),
            StreamClaim::Expired(None)
        ));
        assert!(session.page.bus.closed().is_cancelled());
    }

    #[test]
    fn an_expired_detached_session_cannot_be_revived_before_the_cleanup_sweep() {
        let security = security();
        let (session, _) = security.create_session(&HeaderMap::new()).unwrap();
        let owner = owner_headers(&session);
        let guard = attached(security.claim_stream(&owner, &session.id).unwrap());
        drop(guard);
        let detached = session.streams.lock().unwrap().detached_since;
        assert!(matches!(
            security
                .claim_stream_at(
                    &owner,
                    &session.id,
                    Some(detached + DETACHED_SESSION_LIFETIME)
                )
                .unwrap(),
            StreamClaim::Expired(Some(_))
        ));
        assert!(security.expired_sessions().is_empty());
        assert!(security.session(&owner, &session.id, false).is_err());
        assert!(session.page.bus.closed().is_cancelled());
    }

    #[test]
    fn authentication_rejects_missing_credentials_and_rebinding_host() {
        let security = security();
        let mut headers = HeaderMap::new();
        headers.insert("host", "tt.example".parse().unwrap());
        assert_eq!(
            security.authenticate(&headers),
            Err(StatusCode::UNAUTHORIZED)
        );
        let basic =
            base64::engine::general_purpose::STANDARD.encode("tt:a-long-password-for-tests-only");
        headers.insert("authorization", format!("Basic {basic}").parse().unwrap());
        assert!(security.authenticate(&headers).is_ok());
        headers.insert("host", "evil.example".parse().unwrap());
        assert_eq!(
            security.authenticate(&headers),
            Err(StatusCode::MISDIRECTED_REQUEST)
        );
    }
}
