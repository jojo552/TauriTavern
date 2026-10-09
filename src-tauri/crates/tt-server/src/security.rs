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
/// A page must open its event stream promptly; afterwards the stream itself keeps it alive.
const UNCONNECTED_SESSION_LIFETIME: Duration = Duration::from_secs(120);

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
    created: Instant,
    pub page: PageState,
    pub event_connected: std::sync::atomic::AtomicBool,
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
            created: Instant::now(),
            page: PageState::default(),
            event_connected: std::sync::atomic::AtomicBool::new(false),
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
        let session = self
            .sessions
            .lock()
            .expect("session registry poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| {
                CommandError::Unauthorized("Browser session expired; reload the page".into())
            })?;
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
        Ok(session)
    }

    pub fn remove_session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .remove(id)
    }
    pub fn expired_sessions(&self) -> Vec<Arc<Session>> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        let ids: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| {
                !session
                    .event_connected
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && session.created.elapsed() > UNCONNECTED_SESSION_LIFETIME
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| sessions.remove(&id))
            .collect()
    }
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
