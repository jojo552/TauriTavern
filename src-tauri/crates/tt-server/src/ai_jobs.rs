//! Cookie-owned completion jobs. Pages own attachments, never the upstream task or chat storage.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tt_application::{
    dto::chat_completion_dto::{
        ChatCompletionGenerateRequestDto, ChatCompletionStreamEventDto,
        ChatCompletionStreamStatusDto,
    },
    services::chat_completion_service::ChatCompletionService,
};
use tt_runtime::CoreServices;

use crate::{
    ai::{arg, client_id, ensure_endpoint_granted},
    error::CommandError,
    security::Session,
};

const JOB_TTL: Duration = Duration::from_secs(2 * 60 * 60);
// A password-protected single-user host: capacity failure does not evict unacknowledged work.
const MAX_JOBS: usize = 16;
const MAX_PRESENTATION_BYTES: usize = 32 * 1024;
const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 32 * 1024;

#[derive(Default)]
pub struct BrowserAiJobs {
    entries: Mutex<HashMap<(String, String), Entry>>,
}

struct Entry {
    job: Arc<Job>,
    attached: Option<Arc<Attachment>>,
    created: Instant,
}

struct Attachment {
    page: String,
    detached: CancellationToken,
}

struct OwnedJob {
    job: Arc<Job>,
    attachment: Arc<Attachment>,
}

struct Job {
    server_id: String,
    stream: bool,
    presentation: Option<Value>,
    state: Mutex<JobState>,
    changed: Notify,
    cancel: CancellationToken,
}

// StreamReady is only initialization state. The shared stream owns its events and terminal status.
enum JobState {
    Running,
    StreamReady,
    Complete(Result<Value, CommandError>),
}

// Acknowledgement releases a published terminal result; it must not interrupt provider cleanup.
enum ReleaseReason {
    Acknowledge,
    Cancel,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JobDescriptor<'a> {
    job_id: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation: Option<&'a Value>,
}

#[derive(Serialize)]
struct JobRead<'a> {
    stream: bool,
    status: ChatCompletionStreamStatusDto,
    events: Vec<ChatCompletionStreamEventDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a CommandError>,
}

fn to_json(value: impl Serialize) -> Result<Value, CommandError> {
    serde_json::to_value(value)
        .map_err(|error| CommandError::InternalServerError(error.to_string()))
}

impl Entry {
    fn expired_at(&self, now: Instant) -> bool {
        now.duration_since(self.created) >= JOB_TTL
    }

    fn claim(&mut self, session: &Session) -> Result<(), CommandError> {
        // Page closure revokes its lease at the lifecycle source, even before async cleanup.
        if self
            .attached
            .as_ref()
            .is_some_and(|attached| attached.detached.is_cancelled())
        {
            self.detach();
        }
        if let Some(attached) = &self.attached {
            if attached.page != session.id {
                return Err(CommandError::Conflict(
                    "Completion job is attached to another live page".into(),
                ));
            }
        } else {
            self.attached = Some(Arc::new(Attachment {
                page: session.id.clone(),
                detached: session.page.bus.closed().child_token(),
            }));
        }
        Ok(())
    }

    fn matches(&self, owned: &OwnedJob) -> bool {
        Arc::ptr_eq(&self.job, &owned.job)
            && self
                .attached
                .as_ref()
                .is_some_and(|attached| Arc::ptr_eq(attached, &owned.attachment))
            && !owned.attachment.detached.is_cancelled()
    }

    fn detach(&mut self) {
        if let Some(attached) = self.attached.take() {
            attached.detached.cancel();
        }
    }
}

impl Job {
    fn descriptor(&self, client: &str) -> Result<Value, CommandError> {
        to_json(JobDescriptor {
            job_id: client,
            stream: self.stream,
            presentation: self.presentation.as_ref(),
        })
    }

    async fn wait_ready(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !matches!(
                *self.state.lock().expect("AI job state poisoned"),
                JobState::Running
            ) {
                return;
            }
            notified.await;
        }
    }

    fn complete(&self, result: Result<Value, CommandError>) {
        *self.state.lock().expect("AI job state poisoned") =
            JobState::Complete(bounded_result(result));
        self.changed.notify_waiters();
    }

    async fn status(
        &self,
        service: &ChatCompletionService,
    ) -> Result<ChatCompletionStreamStatusDto, CommandError> {
        {
            let state = self.state.lock().expect("AI job state poisoned");
            match &*state {
                JobState::Running => return Ok(ChatCompletionStreamStatusDto::Running),
                JobState::Complete(result) => return Ok(result_status(result)),
                JobState::StreamReady => {}
            }
        }
        Ok(service.stream_status(&self.server_id).await?)
    }

    async fn read(
        &self,
        service: &ChatCompletionService,
        after_seq: u64,
    ) -> Result<Value, CommandError> {
        if !self.stream && after_seq != 0 {
            return Err(CommandError::BadRequest(
                "Non-stream completion job cursor must be zero".into(),
            ));
        }
        self.wait_ready().await;
        {
            let state = self.state.lock().expect("AI job state poisoned");
            if let JobState::Complete(result) = &*state {
                if after_seq != 0 {
                    return Err(CommandError::BadRequest(
                        "Completion job cursor exceeds delivered sequence zero".into(),
                    ));
                }
                return to_json(JobRead {
                    stream: self.stream,
                    status: result_status(result),
                    events: Vec::new(),
                    result: result.as_ref().ok(),
                    error: result.as_ref().err(),
                });
            }
        }
        let read = service.read_stream(&self.server_id, after_seq).await?;
        to_json(JobRead {
            stream: true,
            status: read.status,
            events: read.events,
            result: None,
            error: None,
        })
    }
}

fn result_status(result: &Result<Value, CommandError>) -> ChatCompletionStreamStatusDto {
    match result {
        Ok(_) => ChatCompletionStreamStatusDto::Done,
        Err(CommandError::Cancelled(_)) => ChatCompletionStreamStatusDto::Cancelled,
        Err(_) => ChatCompletionStreamStatusDto::Error,
    }
}

fn bounded_result(result: Result<Value, CommandError>) -> Result<Value, CommandError> {
    let (bytes, limit) = match &result {
        Ok(value) => (serde_json::to_vec(value), MAX_RESULT_BYTES),
        Err(error) => (serde_json::to_vec(error), MAX_ERROR_BYTES),
    };
    match bytes {
        Ok(bytes) if bytes.len() <= limit => result,
        Ok(_) => Err(CommandError::InternalServerError(
            "Completion job result exceeded its retention limit".into(),
        )),
        Err(error) => Err(CommandError::InternalServerError(format!(
            "Cannot serialize completion job result: {error}"
        ))),
    }
}

fn presentation(args: &Value) -> Result<Option<Value>, CommandError> {
    let Some(value) = args.get("presentation") else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err(CommandError::BadRequest(
            "Completion job presentation must be an object".into(),
        ));
    }
    if serde_json::to_vec(value)
        .map_err(|error| CommandError::BadRequest(error.to_string()))?
        .len()
        > MAX_PRESENTATION_BYTES
    {
        return Err(CommandError::BadRequest(
            "Completion job presentation exceeds 32 KiB".into(),
        ));
    }
    Ok(Some(value.clone()))
}

fn require_open(session: &Session) -> Result<(), CommandError> {
    if session.page.bus.closed().is_cancelled() {
        Err(CommandError::Unauthorized("Browser page closed".into()))
    } else {
        Ok(())
    }
}

fn attachment_closed() -> CommandError {
    CommandError::Unauthorized("Completion job attachment closed".into())
}

impl BrowserAiJobs {
    pub async fn dispatch(
        &self,
        services: &CoreServices,
        session: &Session,
        command: &str,
        args: &Value,
    ) -> Result<Option<Value>, CommandError> {
        let value = match command {
            "start_chat_completion_job" => {
                let client = client_id(args, "jobId")?;
                let dto: ChatCompletionGenerateRequestDto = arg(args, "dto")?;
                let presentation = presentation(args)?;
                let stream = match dto.payload.get("stream") {
                    None => false,
                    Some(value) => value.as_bool().ok_or_else(|| {
                        CommandError::BadRequest("dto.stream must be a boolean".into())
                    })?,
                };
                ensure_endpoint_granted(
                    services,
                    services
                        .chat_completion_service
                        .resolve_generate_user_endpoint(&dto)?,
                )
                .await?;
                let job = self.register(
                    session,
                    client.clone(),
                    stream,
                    presentation,
                    Instant::now(),
                )?;
                let reply = job.descriptor(&client)?;
                let service = services.chat_completion_service.clone();
                // HTTP response loss cannot drop the generation future. Only explicit host cleanup
                // or user cancellation owns this token; detaching a page never cancels it.
                tokio::spawn(async move {
                    if job.cancel.is_cancelled() {
                        return;
                    }
                    if stream {
                        match service
                            .start_replayable_stream(job.server_id.clone(), dto, job.cancel.clone())
                            .await
                        {
                            Ok(()) => {
                                *job.state.lock().expect("AI job state poisoned") =
                                    JobState::StreamReady;
                                job.changed.notify_waiters();
                            }
                            Err(error) => job.complete(Err(error.into())),
                        }
                    } else {
                        let generation = service.generate_request(&job.server_id, dto);
                        tokio::pin!(generation);
                        let result = tokio::select! {
                            biased;
                            _ = job.cancel.cancelled() => {
                                // The generation future is not polled again. Remove any registered
                                // shared cancellation entry before dropping it (also safe if its
                                // registration was still awaiting the registry lock).
                                service.cancel_generation(&job.server_id).await;
                                Err(tt_application::errors::ApplicationError::Cancelled(
                                    "Completion job cancelled".into(),
                                ))
                            }
                            result = &mut generation => result,
                        };
                        job.complete(result.map_err(CommandError::from));
                    }
                });
                reply
            }
            "list_chat_completion_jobs" => {
                let entries = self.entries.lock().expect("AI jobs table poisoned");
                require_open(session)?;
                let now = Instant::now();
                let descriptors = entries
                    .iter()
                    .filter(|((browser, _), entry)| {
                        browser == &session.browser
                            && !entry.expired_at(now)
                            && entry.job.presentation.is_some()
                            && entry.attached.as_ref().is_none_or(|attached| {
                                attached.detached.is_cancelled() || attached.page == session.id
                            })
                    })
                    .map(|((_, client), entry)| entry.job.descriptor(client))
                    .collect::<Result<Vec<_>, _>>()?;
                Value::Array(descriptors)
            }
            "attach_chat_completion_job" => {
                let client = client_id(args, "jobId")?;
                self.attach(session, &client)?
            }
            "read_chat_completion_job" => {
                let client = client_id(args, "jobId")?;
                let after_seq: u64 = arg(args, "afterSeq")?;
                let owned = self.owned(session, &client)?;
                let value = tokio::select! {
                    biased;
                    _ = owned.attachment.detached.cancelled() => return Err(attachment_closed()),
                    result = owned.job.read(&services.chat_completion_service, after_seq) => result?,
                };
                // Validate this exact lease, not just the page/id: a late read must not deliver
                // into a new attachment or a new job that happens to reuse the same client id.
                self.validate_owned(session, &client, &owned)?;
                value
            }
            "acknowledge_chat_completion_job" => {
                let client = client_id(args, "jobId")?;
                let owned = self.owned(session, &client)?;
                if owned.job.status(&services.chat_completion_service).await?
                    == ChatCompletionStreamStatusDto::Running
                {
                    return Err(CommandError::Conflict(
                        "Cannot acknowledge a running completion job".into(),
                    ));
                }
                let job = self.remove_owned(session, &client, &owned)?;
                Self::release(&job, services, ReleaseReason::Acknowledge).await;
                // A lost ACK response can subsequently produce NotFound. The persisted chat's
                // completion marker, not a second server-side acknowledgement history, proves
                // that the consumer already published this result.
                Value::Null
            }
            "cancel_chat_completion_job" => {
                let client = client_id(args, "jobId")?;
                let owned = self.owned(session, &client)?;
                let job = self.remove_owned(session, &client, &owned)?;
                Self::release(&job, services, ReleaseReason::Cancel).await;
                Value::Null
            }
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    fn register(
        &self,
        session: &Session,
        client: String,
        stream: bool,
        presentation: Option<Value>,
        now: Instant,
    ) -> Result<Arc<Job>, CommandError> {
        let mut entries = self.entries.lock().expect("AI jobs table poisoned");
        require_open(session)?;
        let key = (session.browser.clone(), client);
        if entries.contains_key(&key) {
            return Err(CommandError::Conflict(
                "Completion job already exists".into(),
            ));
        }
        if let Some(chat_ref) = presentation.as_ref().and_then(|value| value.get("chatRef"))
            && entries.iter().any(|((browser, _), entry)| {
                browser == &session.browser
                    && entry
                        .job
                        .presentation
                        .as_ref()
                        .and_then(|value| value.get("chatRef"))
                        == Some(chat_ref)
            })
        {
            return Err(CommandError::Conflict(
                "This chat already has an unacknowledged completion job".into(),
            ));
        }
        if entries.len() >= MAX_JOBS {
            return Err(CommandError::TooManyRequests(
                "Too many retained completion jobs; acknowledge or cancel existing jobs".into(),
            ));
        }
        let job = Arc::new(Job {
            server_id: format!("web-job-{}", uuid::Uuid::new_v4().simple()),
            stream,
            presentation,
            state: Mutex::new(JobState::Running),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
        });
        entries.insert(
            key,
            Entry {
                job: job.clone(),
                attached: Some(Arc::new(Attachment {
                    page: session.id.clone(),
                    detached: session.page.bus.closed().child_token(),
                })),
                created: now,
            },
        );
        Ok(job)
    }

    fn attach(&self, session: &Session, client: &str) -> Result<Value, CommandError> {
        let mut entries = self.entries.lock().expect("AI jobs table poisoned");
        require_open(session)?;
        let entry = entries
            .get_mut(&(session.browser.clone(), client.to_string()))
            .filter(|entry| !entry.expired_at(Instant::now()))
            .ok_or_else(|| CommandError::NotFound(format!("Completion job not found: {client}")))?;
        entry.claim(session)?;
        entry.job.descriptor(client)
    }

    fn owned(&self, session: &Session, client: &str) -> Result<OwnedJob, CommandError> {
        let entries = self.entries.lock().expect("AI jobs table poisoned");
        let entry = Self::owned_entry(&entries, session, client)?;
        Ok(OwnedJob {
            job: entry.job.clone(),
            attachment: entry.attached.as_ref().expect("checked attachment").clone(),
        })
    }

    fn owned_entry<'a>(
        entries: &'a HashMap<(String, String), Entry>,
        session: &Session,
        client: &str,
    ) -> Result<&'a Entry, CommandError> {
        require_open(session)?;
        let entry = entries
            .get(&(session.browser.clone(), client.to_string()))
            .filter(|entry| !entry.expired_at(Instant::now()))
            .ok_or_else(|| CommandError::NotFound(format!("Completion job not found: {client}")))?;
        if entry
            .attached
            .as_ref()
            .is_none_or(|attached| attached.page != session.id)
        {
            return Err(CommandError::Conflict(
                "Completion job is not attached to this page".into(),
            ));
        }
        Ok(entry)
    }

    fn validate_owned(
        &self,
        session: &Session,
        client: &str,
        owned: &OwnedJob,
    ) -> Result<(), CommandError> {
        let entries = self.entries.lock().expect("AI jobs table poisoned");
        if !Self::owned_entry(&entries, session, client)?.matches(owned) {
            return Err(attachment_closed());
        }
        Ok(())
    }

    fn remove_owned(
        &self,
        session: &Session,
        client: &str,
        owned: &OwnedJob,
    ) -> Result<Arc<Job>, CommandError> {
        let mut entries = self.entries.lock().expect("AI jobs table poisoned");
        if !Self::owned_entry(&entries, session, client)?.matches(owned) {
            return Err(attachment_closed());
        }
        let mut entry = entries
            .remove(&(session.browser.clone(), client.to_string()))
            .expect("checked job entry");
        entry.detach();
        Ok(entry.job)
    }

    pub fn detach_page(&self, page: &str) {
        let mut entries = self.entries.lock().expect("AI jobs table poisoned");
        for entry in entries.values_mut() {
            if entry
                .attached
                .as_ref()
                .is_some_and(|attached| attached.page == page)
            {
                entry.detach();
            }
        }
    }

    fn take_expired(&self, now: Instant) -> Vec<Arc<Job>> {
        let mut entries = self.entries.lock().expect("AI jobs table poisoned");
        let keys: Vec<_> = entries
            .iter()
            .filter(|(_, entry)| entry.expired_at(now))
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .map(|key| {
                let mut entry = entries.remove(&key).expect("checked expired job");
                entry.detach();
                entry.job
            })
            .collect()
    }

    pub async fn expire(&self, services: &CoreServices) {
        for job in self.take_expired(Instant::now()) {
            Self::release(&job, services, ReleaseReason::Cancel).await;
        }
    }

    async fn release(job: &Job, services: &CoreServices, reason: ReleaseReason) {
        let cancel = matches!(reason, ReleaseReason::Cancel);
        if cancel {
            job.cancel.cancel();
        }
        if job.stream {
            // Shared close signals cancellation only while running, not for a terminal stream.
            services
                .chat_completion_service
                .close_stream(&job.server_id)
                .await;
        } else if cancel {
            services
                .chat_completion_service
                .cancel_generation(&job.server_id)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, task::Context};

    use http::HeaderMap;
    use serde_json::json;

    use super::*;
    use crate::security::Security;

    fn pages() -> (Arc<Session>, Arc<Session>, Arc<Session>) {
        let security = Security::new(
            "tt",
            "a-long-password-for-tests-only",
            &url::Url::parse("https://tt.example").unwrap(),
        )
        .unwrap();
        let authenticated = || {
            let login = security
                .login(&HeaderMap::new(), "tt", "a-long-password-for-tests-only")
                .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(
                "cookie",
                format!(
                    "tt_browser={}; tt_auth={}",
                    login.authentication.browser, login.authentication.id
                )
                .parse()
                .unwrap(),
            );
            headers
        };
        let headers = authenticated();
        let first = security.create_session(&headers).unwrap();
        let second = security.create_session(&headers).unwrap();
        let other_browser = security.create_session(&authenticated()).unwrap();
        (first, second, other_browser)
    }

    #[test]
    fn ownership_and_exact_attachment_fence_late_reads_and_removals() {
        let (first, second, other_browser) = pages();
        let jobs = BrowserAiJobs::default();
        let presentation = json!({
            "chatRef": { "kind": "character", "characterId": "one", "fileName": "chat" }
        });
        let job = jobs
            .register(
                &first,
                "job".into(),
                false,
                Some(presentation.clone()),
                Instant::now(),
            )
            .unwrap();
        assert_eq!(
            job.descriptor("job").unwrap(),
            json!({ "jobId": "job", "stream": false, "presentation": presentation })
        );
        assert!(matches!(
            jobs.register(
                &second,
                "same-chat".into(),
                false,
                Some(presentation),
                Instant::now(),
            ),
            Err(CommandError::Conflict(_))
        ));
        let old = jobs.owned(&first, "job").unwrap();
        jobs.attach(&first, "job").unwrap();
        assert!(jobs.validate_owned(&first, "job", &old).is_ok());
        assert!(matches!(
            jobs.attach(&second, "job"),
            Err(CommandError::Conflict(_))
        ));
        assert!(matches!(
            jobs.attach(&other_browser, "job"),
            Err(CommandError::NotFound(_))
        ));
        assert!(matches!(
            jobs.register(&first, "job".into(), false, None, Instant::now()),
            Err(CommandError::Conflict(_))
        ));

        jobs.detach_page(&first.id);
        assert!(old.attachment.detached.is_cancelled());
        assert!(!first.page.bus.closed().is_cancelled());
        assert!(!job.cancel.is_cancelled());
        jobs.attach(&first, "job").unwrap();
        assert!(jobs.validate_owned(&first, "job", &old).is_err());
        assert!(jobs.remove_owned(&first, "job", &old).is_err());
        let current = jobs.owned(&first, "job").unwrap();
        jobs.remove_owned(&first, "job", &current).unwrap();
        assert!(matches!(
            jobs.attach(&first, "job"),
            Err(CommandError::NotFound(_))
        ));
        jobs.register(&first, "job".into(), false, None, Instant::now())
            .unwrap();
        assert!(jobs.remove_owned(&first, "job", &current).is_err());
        assert!(jobs.owned(&first, "job").is_ok());

        let closing = jobs.owned(&first, "job").unwrap();
        first.page.bus.close();
        assert!(closing.attachment.detached.is_cancelled());
        assert!(!closing.job.cancel.is_cancelled());
        jobs.attach(&second, "job").unwrap();
        // Old-page cleanup arriving after the new attachment must not revoke the new reader.
        jobs.detach_page(&first.id);
        assert!(jobs.owned(&first, "job").is_err());
        assert!(
            !jobs
                .owned(&second, "job")
                .unwrap()
                .attachment
                .detached
                .is_cancelled()
        );
    }

    #[test]
    fn global_capacity_and_creation_ttl_do_not_evict_or_extend_work() {
        let (first, second, other_browser) = pages();
        let jobs = BrowserAiJobs::default();
        let now = Instant::now();
        for index in 0..MAX_JOBS {
            jobs.register(
                if index % 2 == 0 {
                    &first
                } else {
                    &other_browser
                },
                index.to_string(),
                false,
                None,
                now,
            )
            .unwrap();
        }
        assert!(matches!(
            jobs.register(&second, "extra".into(), false, None, now),
            Err(CommandError::TooManyRequests(_))
        ));
        jobs.detach_page(&first.id);
        jobs.attach(&second, "0").unwrap();
        let reader = jobs.owned(&second, "0").unwrap();
        assert!(
            jobs.take_expired(now + JOB_TTL - Duration::from_nanos(1))
                .is_empty()
        );
        let expired = jobs.take_expired(now + JOB_TTL);
        assert_eq!(expired.len(), MAX_JOBS);
        assert!(reader.attachment.detached.is_cancelled());
        assert!(jobs.entries.lock().unwrap().is_empty());
        jobs.register(&second, "extra".into(), false, None, now + JOB_TTL)
            .unwrap();
    }

    #[tokio::test]
    async fn nonstream_completion_wakes_all_waiters_and_bounds_typed_errors() {
        let job = Job {
            server_id: "server".into(),
            stream: false,
            presentation: None,
            state: Mutex::new(JobState::Running),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
        };
        let mut first = Box::pin(job.wait_ready());
        let mut second = Box::pin(job.wait_ready());
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(second.as_mut().poll(&mut context).is_pending());
        let error = CommandError::UpstreamFailure(
            tt_domain::models::upstream_failure::UpstreamFailure::network(
                "network.timeout",
                Some("https://example.org".into()),
                "errors.network.timeout",
            ),
        );
        job.complete(Err(error));
        assert!(first.as_mut().poll(&mut context).is_ready());
        assert!(second.as_mut().poll(&mut context).is_ready());
        let state = job.state.lock().unwrap();
        let JobState::Complete(result) = &*state else {
            panic!("expected completion");
        };
        let wire = to_json(JobRead {
            stream: false,
            status: result_status(result),
            events: Vec::new(),
            result: result.as_ref().ok(),
            error: result.as_ref().err(),
        })
        .unwrap();
        assert_eq!(wire["status"], "error");
        assert_eq!(wire["error"]["UpstreamFailure"]["code"], "network.timeout");
        assert!(wire.get("result").is_none());
        drop(state);

        for oversized in [
            Ok(json!("\\".repeat(MAX_RESULT_BYTES / 2))),
            Err(CommandError::BadRequest("\\".repeat(MAX_ERROR_BYTES / 2))),
        ] {
            let retained = bounded_result(oversized).unwrap_err();
            assert!(matches!(retained, CommandError::InternalServerError(_)));
            assert!(serde_json::to_vec(&retained).unwrap().len() <= MAX_ERROR_BYTES);
        }
    }
}
