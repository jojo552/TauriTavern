//! Cookie-only, single-user authentication. Security serializes this registry with page leases.
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use http::HeaderMap;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const AUTH_COOKIE: &str = "tt_auth";
pub const AUTH_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_LOGINS: usize = 64;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILURES: usize = 16;

pub struct Authentication {
    pub id: String,
    pub browser: String,
    pub expires_at: Instant,
    pub revoked: CancellationToken,
}

impl Authentication {
    pub fn active_at(&self, now: Instant) -> bool {
        now < self.expires_at && !self.revoked.is_cancelled()
    }
}

#[derive(Debug)]
pub enum LoginError {
    Credentials,
    RateLimited,
    Capacity,
}

pub struct Logins {
    credentials: [u8; 32],
    entries: HashMap<String, Arc<Authentication>>,
    failures: VecDeque<Instant>,
}

fn credential_digest(username: &str, password: &str) -> [u8; 32] {
    // Length delimiting prevents a ':' in supplied credentials changing their partition.
    let mut digest = Sha256::new();
    digest.update((username.len() as u64).to_be_bytes());
    digest.update(username.as_bytes());
    digest.update(password.as_bytes());
    digest.finalize().into()
}

impl Logins {
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            credentials: credential_digest(username, password),
            entries: HashMap::new(),
            failures: VecDeque::new(),
        }
    }

    pub fn authenticate(&self, headers: &HeaderMap, now: Instant) -> Option<Arc<Authentication>> {
        let id = cookie_token(headers, AUTH_COOKIE)?;
        let entry = self.entries.get(&id)?;
        if !entry.active_at(now)
            || cookie_token(headers, "tt_browser").as_deref() != Some(entry.browser.as_str())
        {
            return None;
        }
        Some(entry.clone())
    }

    pub fn login(
        &mut self,
        headers: &HeaderMap,
        username: &str,
        password: &str,
        now: Instant,
    ) -> Result<Arc<Authentication>, LoginError> {
        while self
            .failures
            .front()
            .is_some_and(|at| now.duration_since(*at) >= FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        if self.failures.len() >= MAX_FAILURES {
            return Err(LoginError::RateLimited);
        }
        if !constant_time_equal(&credential_digest(username, password), &self.credentials) {
            self.failures.push_back(now);
            return Err(LoginError::Credentials);
        }
        self.expire(now);
        let browser = cookie_token(headers, "tt_browser").unwrap_or_else(random_token);
        self.entries.retain(|_, entry| {
            if entry.browser == browser {
                entry.revoked.cancel();
                false
            } else {
                true
            }
        });
        if self.entries.len() >= MAX_LOGINS {
            return Err(LoginError::Capacity);
        }
        // An explicit successful login replaces the lease. Token and Cookie both last exactly
        // seven days, rather than extending Cookie Max-Age on a reused fixed-deadline token.
        let entry = Arc::new(Authentication {
            id: random_token(),
            browser,
            expires_at: now + AUTH_LIFETIME,
            revoked: CancellationToken::new(),
        });
        self.entries.insert(entry.id.clone(), entry.clone());
        Ok(entry)
    }

    pub fn logout(&mut self, headers: &HeaderMap) {
        if let Some(id) = cookie_token(headers, AUTH_COOKIE)
            && let Some(entry) = self.entries.remove(&id)
        {
            // Revocation only: a missing/corrupt identity Cookie must not leave an already-open
            // stream authorized after logout. Private access still requires both Cookies.
            entry.revoked.cancel();
        }
    }

    pub fn expire(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            if entry.active_at(now) {
                true
            } else {
                entry.revoked.cancel();
                false
            }
        });
    }
}

pub fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (l, r)| difference | (l ^ r))
        == 0
}

pub fn cookie_token(headers: &HeaderMap, cookie: &str) -> Option<String> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == cookie && value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit()))
                .then(|| value.to_string())
        })
}
