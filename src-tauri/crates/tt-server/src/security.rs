use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use http::{HeaderMap, StatusCode};

use crate::{
    auth::{
        AUTH_COOKIE, AUTH_LIFETIME, Authentication, LoginError, Logins, constant_time_equal,
        cookie_token, random_token,
    },
    error::CommandError,
    host::PageState,
};

const COOKIE: &str = "tt_browser";
const BROWSER_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_SESSIONS: usize = 64;
const UNCONNECTED_SESSION_LIFETIME: Duration = Duration::from_secs(120);
const DETACHED_SESSION_LIFETIME: Duration = Duration::from_secs(10 * 60);

pub struct Security {
    pub public_origin: String,
    authority: String,
    secure_cookie: bool,
    // Authentication changes and page admission share this ordering. A middleware-approved
    // request must still prove its current lease inside this lock before registering a page.
    registry: Mutex<Registry>,
}

struct Registry {
    logins: Logins,
    sessions: HashMap<String, Arc<Session>>,
}

pub struct LoginResult {
    pub authentication: Arc<Authentication>,
    pub cookies: [String; 2],
    pub closed_pages: Vec<Arc<Session>>,
}

pub struct Session {
    pub id: String,
    pub csrf: String,
    pub browser: String,
    pub authentication: Arc<Authentication>,
    pub page: PageState,
    streams: Mutex<StreamState>,
}

impl Session {
    pub fn active_at(&self, now: Instant) -> bool {
        self.authentication.active_at(now) && !self.page.bus.closed().is_cancelled()
    }
}

struct StreamState {
    attached: usize,
    ever_attached: bool,
    detached_since: Instant,
}

impl StreamState {
    fn expired_at(&self, now: Instant) -> bool {
        let lifetime = if self.ever_attached {
            DETACHED_SESSION_LIFETIME
        } else {
            UNCONNECTED_SESSION_LIFETIME
        };
        self.attached == 0 && now.duration_since(self.detached_since) >= lifetime
    }
}

/// Held by the SSE body. Dropping a connection starts page grace, not authentication renewal.
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
    Expired(Option<Arc<Session>>),
}

impl Registry {
    fn remove_closed_pages(&mut self, now: Instant, include_grace: bool) -> Vec<Arc<Session>> {
        let ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                !session.active_at(now)
                    || (include_grace
                        && session
                            .streams
                            .lock()
                            .expect("session stream state poisoned")
                            .expired_at(now))
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let session = self.sessions.remove(&id)?;
                // Synchronous revocation at the source stops readers before async resource cleanup.
                session.page.bus.close();
                Some(session)
            })
            .collect()
    }
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
            public_origin: origin.origin().ascii_serialization(),
            authority: authority.to_string(),
            secure_cookie: origin.scheme() == "https",
            registry: Mutex::new(Registry {
                logins: Logins::new(username, password),
                sessions: HashMap::new(),
            }),
        })
    }

    /// Never trust forwarded host/origin or infer an authentication mode from client metadata.
    pub fn check_host(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        if headers.get("host").and_then(|value| value.to_str().ok()) != Some(&self.authority) {
            return Err(StatusCode::MISDIRECTED_REQUEST);
        }
        Ok(())
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Arc<Authentication>, StatusCode> {
        self.check_host(headers)?;
        self.registry
            .lock()
            .expect("security registry poisoned")
            .logins
            .authenticate(headers, Instant::now())
            .ok_or(StatusCode::UNAUTHORIZED)
    }

    pub fn login(
        &self,
        headers: &HeaderMap,
        username: &str,
        password: &str,
    ) -> Result<LoginResult, LoginError> {
        self.login_at(headers, username, password, Instant::now())
    }

    fn login_at(
        &self,
        headers: &HeaderMap,
        username: &str,
        password: &str,
        now: Instant,
    ) -> Result<LoginResult, LoginError> {
        let mut registry = self.registry.lock().expect("security registry poisoned");
        let authentication = registry.logins.login(headers, username, password, now)?;
        let closed_pages = registry.remove_closed_pages(now, false);
        let cookies = [
            self.cookie(AUTH_COOKIE, &authentication.id, AUTH_LIFETIME.as_secs()),
            self.cookie(COOKIE, &authentication.browser, BROWSER_LIFETIME.as_secs()),
        ];
        Ok(LoginResult {
            authentication,
            cookies,
            closed_pages,
        })
    }

    pub fn logout(&self, headers: &HeaderMap) -> Vec<Arc<Session>> {
        let mut registry = self.registry.lock().expect("security registry poisoned");
        let now = Instant::now();
        registry.logins.logout(headers);
        registry.logins.expire(now);
        registry.remove_closed_pages(now, false)
    }

    fn cookie(&self, name: &str, value: &str, max_age: u64) -> String {
        format!(
            "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
            if self.secure_cookie { "; Secure" } else { "" }
        )
    }

    pub fn clear_auth_cookie(&self) -> String {
        self.cookie(AUTH_COOKIE, "", 0)
    }

    /// Existing private page APIs preserve optional Origin/direct navigation semantics.
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
            .is_some_and(|value| value != "same-origin" && value != "none")
        {
            return Err(CommandError::Unauthorized(
                "Cross-site requests are not allowed".into(),
            ));
        }
        Ok(())
    }

    /// Auth POSTs need an exact Origin even on HTTP/old browsers without Fetch Metadata.
    pub fn auth_origin(
        &self,
        headers: &HeaderMap,
        require_origin: bool,
    ) -> Result<(), CommandError> {
        let origin = headers.get("origin").and_then(|value| value.to_str().ok());
        if (require_origin && origin != Some(self.public_origin.as_str()))
            || (headers.contains_key("origin") && origin != Some(self.public_origin.as_str()))
            || headers
                .get("sec-fetch-site")
                .is_some_and(|value| value != "same-origin")
        {
            return Err(CommandError::Unauthorized(
                "Authentication requires a same-origin request".into(),
            ));
        }
        Ok(())
    }

    pub fn create_session(&self, headers: &HeaderMap) -> Result<Arc<Session>, CommandError> {
        self.same_origin(headers)?;
        let mut registry = self.registry.lock().expect("security registry poisoned");
        let now = Instant::now();
        let authentication = current_authentication(&registry, headers, now)?;
        if registry.sessions.len() >= MAX_SESSIONS {
            return Err(CommandError::TooManyRequests(
                "Too many browser sessions; close unused tabs".into(),
            ));
        }
        let session = Arc::new(Session {
            id: random_token(),
            csrf: random_token(),
            browser: authentication.browser.clone(),
            authentication,
            page: PageState::default(),
            streams: Mutex::new(StreamState {
                attached: 0,
                ever_attached: false,
                detached_since: now,
            }),
        });
        registry
            .sessions
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub fn session(
        &self,
        headers: &HeaderMap,
        id: &str,
        require_csrf: bool,
    ) -> Result<Arc<Session>, CommandError> {
        self.same_origin(headers)?;
        let registry = self.registry.lock().expect("security registry poisoned");
        let now = Instant::now();
        let authentication = current_authentication(&registry, headers, now)?;
        let session = registry.sessions.get(id).ok_or_else(|| {
            CommandError::Unauthorized("Browser session expired; reload the page".into())
        })?;
        authorize_session(headers, session, &authentication, require_csrf)?;
        if !session.active_at(now)
            || session
                .streams
                .lock()
                .expect("session stream state poisoned")
                .expired_at(now)
        {
            return Err(CommandError::Unauthorized(
                "Browser session expired; reload the page".into(),
            ));
        }
        Ok(session.clone())
    }

    /// Fixed lock order registry -> streams preserves the original page grace semantics.
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
        let mut registry = self.registry.lock().expect("security registry poisoned");
        let now = now.unwrap_or_else(Instant::now);
        let authentication = current_authentication(&registry, headers, now)?;
        let Some(session) = registry.sessions.get(id).cloned() else {
            return Ok(StreamClaim::Expired(None));
        };
        authorize_session(headers, &session, &authentication, false)?;
        let mut state = session
            .streams
            .lock()
            .expect("session stream state poisoned");
        if !session.active_at(now) || state.expired_at(now) {
            registry.sessions.remove(id);
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
            .registry
            .lock()
            .expect("security registry poisoned")
            .sessions
            .remove(id)?;
        session.page.bus.close();
        Some(session)
    }

    pub fn expired_sessions(&self) -> Vec<Arc<Session>> {
        self.expired_sessions_at(None)
    }

    fn expired_sessions_at(&self, now: Option<Instant>) -> Vec<Arc<Session>> {
        let mut registry = self.registry.lock().expect("security registry poisoned");
        let now = now.unwrap_or_else(Instant::now);
        registry.logins.expire(now);
        registry.remove_closed_pages(now, true)
    }
}

fn current_authentication(
    registry: &Registry,
    headers: &HeaderMap,
    now: Instant,
) -> Result<Arc<Authentication>, CommandError> {
    registry.logins.authenticate(headers, now).ok_or_else(|| {
        CommandError::Unauthorized("Browser authentication expired; log in again".into())
    })
}

fn authorize_session(
    headers: &HeaderMap,
    session: &Session,
    authentication: &Arc<Authentication>,
    require_csrf: bool,
) -> Result<(), CommandError> {
    if browser_cookie(headers).as_deref() != Some(session.browser.as_str())
        || !Arc::ptr_eq(&session.authentication, authentication)
    {
        return Err(CommandError::Unauthorized(
            "Session does not belong to this browser authentication".into(),
        ));
    }
    if require_csrf {
        let csrf = headers
            .get("x-tt-csrf")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !constant_time_equal(csrf.as_bytes(), session.csrf.as_bytes()) {
            return Err(CommandError::Unauthorized("Invalid CSRF token".into()));
        }
    }
    Ok(())
}

pub fn browser_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_token(headers, COOKIE)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn login_headers(security: &Security) -> HeaderMap {
        let login = security
            .login(&HeaderMap::new(), "tt", "a-long-password-for-tests-only")
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("host", "tt.example".parse().unwrap());
        headers.insert(
            "cookie",
            format!(
                "{COOKIE}={}; {AUTH_COOKIE}={}",
                login.authentication.browser, login.authentication.id
            )
            .parse()
            .unwrap(),
        );
        headers
    }

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
        let session = security.create_session(&login_headers(&security)).unwrap();
        let mut headers = owner_headers(&session);
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
        headers.insert("host", "tt.example".parse().unwrap());
        headers.insert(
            "cookie",
            format!(
                "{COOKIE}={}; {AUTH_COOKIE}={}",
                session.browser, session.authentication.id
            )
            .parse()
            .unwrap(),
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
        let session = security.create_session(&login_headers(&security)).unwrap();
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
        let session = security.create_session(&login_headers(&security)).unwrap();
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
        let session = security.create_session(&login_headers(&security)).unwrap();
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
    fn cookie_authentication_rejects_basic_headers_and_rebinding_hosts() {
        let security = security();
        let mut headers = HeaderMap::new();
        headers.insert("host", "tt.example".parse().unwrap());
        headers.insert(
            "authorization",
            "Basic dHQ6YS1sb25nLXBhc3N3b3JkLWZvci10ZXN0cy1vbmx5"
                .parse()
                .unwrap(),
        );
        assert!(matches!(
            security.authenticate(&headers),
            Err(StatusCode::UNAUTHORIZED)
        ));
        let mut headers = login_headers(&security);
        assert!(security.authenticate(&headers).is_ok());
        headers.insert("host", "evil.example".parse().unwrap());
        assert!(matches!(
            security.authenticate(&headers),
            Err(StatusCode::MISDIRECTED_REQUEST)
        ));
        headers.insert("host", "tt.example".parse().unwrap());
        headers.insert(
            "cookie",
            "tt_auth=not-a-token; tt_browser=not-a-token"
                .parse()
                .unwrap(),
        );
        assert!(matches!(
            security.authenticate(&headers),
            Err(StatusCode::UNAUTHORIZED)
        ));
    }

    #[test]
    fn login_limits_are_bounded_and_expiry_is_fixed_not_sliding() {
        let security = security();
        let now = Instant::now();
        for _ in 0..16 {
            assert!(matches!(
                security.login_at(&HeaderMap::new(), "tt", "wrong", now),
                Err(LoginError::Credentials)
            ));
        }
        assert!(matches!(
            security.login_at(
                &HeaderMap::new(),
                "tt",
                "a-long-password-for-tests-only",
                now
            ),
            Err(LoginError::RateLimited)
        ));
        let login = security
            .login_at(
                &HeaderMap::new(),
                "tt",
                "a-long-password-for-tests-only",
                now + Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(
            login.authentication.expires_at,
            now + Duration::from_secs(60) + AUTH_LIFETIME
        );
        for cookie in &login.cookies {
            assert!(cookie.contains("HttpOnly; SameSite=Strict"));
            assert!(cookie.ends_with("; Secure"));
        }
        assert!(login.cookies[0].contains("Max-Age=604800"));
        assert!(login.cookies[1].contains("Max-Age=2592000"));
        for _ in 1..64 {
            security
                .login_at(
                    &HeaderMap::new(),
                    "tt",
                    "a-long-password-for-tests-only",
                    now + Duration::from_secs(60),
                )
                .unwrap();
        }
        assert!(matches!(
            security.login_at(
                &HeaderMap::new(),
                "tt",
                "a-long-password-for-tests-only",
                now + Duration::from_secs(60)
            ),
            Err(LoginError::Capacity)
        ));
    }

    #[test]
    fn logout_and_login_replacement_close_all_pages_but_preserve_browser_identity() {
        let security = security();
        let headers = login_headers(&security);
        let first = security.create_session(&headers).unwrap();
        let second = security.create_session(&headers).unwrap();
        let _stream = attached(security.claim_stream(&headers, &first.id).unwrap());
        let mut logout_headers = headers.clone();
        logout_headers.insert(
            "cookie",
            format!("{AUTH_COOKIE}={}", first.authentication.id)
                .parse()
                .unwrap(),
        );
        let closed = security.logout(&logout_headers);
        assert_eq!(closed.len(), 2);
        assert!(first.authentication.revoked.is_cancelled());
        assert!(first.page.bus.closed().is_cancelled());
        assert!(second.page.bus.closed().is_cancelled());
        assert!(security.create_session(&headers).is_err());
        assert!(security.claim_stream(&headers, &first.id).is_err());
        let login = security
            .login(&headers, "tt", "a-long-password-for-tests-only")
            .unwrap();
        assert_eq!(login.authentication.browser, first.browser);
        assert_ne!(login.authentication.id, first.authentication.id);
        assert!(!security.clear_auth_cookie().contains(COOKIE));
        let mut current = headers.clone();
        current.insert(
            "cookie",
            format!(
                "{COOKIE}={}; {AUTH_COOKIE}={}",
                login.authentication.browser, login.authentication.id
            )
            .parse()
            .unwrap(),
        );
        let page = security.create_session(&current).unwrap();
        let replacement = security
            .login(&current, "tt", "a-long-password-for-tests-only")
            .unwrap();
        assert_eq!(replacement.closed_pages.len(), 1);
        assert!(page.page.bus.closed().is_cancelled());
        assert!(page.authentication.revoked.is_cancelled());
        assert_eq!(replacement.authentication.browser, first.browser);
    }

    #[test]
    fn middleware_approval_cannot_register_or_claim_a_page_after_logout() {
        use std::{sync::Barrier, thread};

        let security = Arc::new(security());
        let headers = login_headers(&security);
        let approved = security.authenticate(&headers).unwrap();
        let page = security.create_session(&headers).unwrap();
        let mut registry = security.registry.lock().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let worker = {
            let security = security.clone();
            let headers = headers.clone();
            let barrier = barrier.clone();
            let page_id = page.id.clone();
            thread::spawn(move || {
                barrier.wait();
                (
                    security.create_session(&headers).is_err(),
                    security.claim_stream(&headers, &page_id).is_err(),
                )
            })
        };
        barrier.wait();
        registry.logins.logout(&headers);
        let closed = registry.remove_closed_pages(Instant::now(), false);
        drop(registry);
        assert!(approved.revoked.is_cancelled());
        assert_eq!(worker.join().unwrap(), (true, true));
        assert_eq!(closed.len(), 1);
        assert!(page.page.bus.closed().is_cancelled());
        assert!(security.expired_sessions().is_empty());
    }

    #[test]
    fn authentication_deadline_revokes_even_attached_pages_without_page_grace() {
        let security = security();
        let headers = login_headers(&security);
        let page = security.create_session(&headers).unwrap();
        let _stream = attached(security.claim_stream(&headers, &page.id).unwrap());
        let expired = security.expired_sessions_at(Some(page.authentication.expires_at));
        assert_eq!(expired.len(), 1);
        assert!(Arc::ptr_eq(&expired[0], &page));
        assert!(page.authentication.revoked.is_cancelled());
        assert!(page.page.bus.closed().is_cancelled());
        assert!(security.session(&headers, &page.id, false).is_err());
        assert!(security.claim_stream(&headers, &page.id).is_err());
    }

    #[test]
    fn authentication_post_requires_exact_origin_and_never_trusts_metadata_instead() {
        let security = security();
        let mut headers = HeaderMap::new();
        assert!(security.auth_origin(&headers, true).is_err());
        assert!(security.auth_origin(&headers, false).is_ok());
        headers.insert("origin", "https://tt.example".parse().unwrap());
        assert!(security.auth_origin(&headers, true).is_ok());
        for value in ["same-site", "cross-site", "none"] {
            headers.insert("sec-fetch-site", value.parse().unwrap());
            assert!(security.auth_origin(&headers, true).is_err());
        }
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(security.auth_origin(&headers, true).is_ok());
        headers.insert("origin", "https://evil.example".parse().unwrap());
        assert!(security.auth_origin(&headers, true).is_err());
        assert!(security.auth_origin(&headers, false).is_err());
    }
}
