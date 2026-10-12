use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, RwLock, watch};
use tokio_util::sync::CancellationToken;
use tt_domain::models::upstream_failure::UpstreamFailure;

use crate::dto::chat_completion_dto::{
    ChatCompletionStreamEventDto, ChatCompletionStreamReadResultDto, ChatCompletionStreamStatusDto,
};
use crate::errors::ApplicationError;

const READ_BATCH_SIZE: usize = 64;
const MAX_REPLAY_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPLAY_FRAMES: usize = 8192;
const PROVIDER_DONE_SENTINEL: &str = "[DONE]";
// Leave room to report an overflow without exceeding the replayable wire-history bound.
const TERMINAL_RESERVE_BYTES: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamRetention {
    Acknowledged,
    Replayable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamAppendOutcome {
    Appended,
    ProviderDone,
    SessionClosed,
}

#[derive(Default)]
pub(super) struct StreamSessionRegistry {
    active: RwLock<HashMap<String, Arc<StreamSession>>>,
}

impl StreamSessionRegistry {
    pub(super) async fn register(
        &self,
        stream_id: &str,
        retention: StreamRetention,
        host_cancel: &CancellationToken,
    ) -> Result<watch::Receiver<bool>, ApplicationError> {
        let mut active = self.active.write().await;
        // Host release can happen while registration waits for this lock. Reject at the source
        // rather than insert a session after its host has already tried to close it.
        if host_cancel.is_cancelled() {
            return Err(tt_domain::errors::DomainError::generation_cancelled_by_user().into());
        }
        if active.contains_key(stream_id) {
            return Err(ApplicationError::Conflict(format!(
                "Chat completion stream already exists: {stream_id}"
            )));
        }

        let (session, cancel) = StreamSession::new(retention);
        active.insert(stream_id.to_string(), Arc::new(session));
        Ok(cancel)
    }

    pub(super) async fn read(
        &self,
        stream_id: &str,
        after_seq: u64,
    ) -> Result<ChatCompletionStreamReadResultDto, ApplicationError> {
        let session = self
            .active
            .read()
            .await
            .get(stream_id)
            .cloned()
            .ok_or_else(|| {
                ApplicationError::NotFound(format!("Chat completion stream not found: {stream_id}"))
            })?;

        session.read(after_seq).await
    }

    pub(super) async fn status(
        &self,
        stream_id: &str,
    ) -> Result<ChatCompletionStreamStatusDto, ApplicationError> {
        let session = self
            .active
            .read()
            .await
            .get(stream_id)
            .cloned()
            .ok_or_else(|| {
                ApplicationError::NotFound(format!("Chat completion stream not found: {stream_id}"))
            })?;
        let status = session.state.lock().await.status;
        Ok(status)
    }

    pub(super) async fn append(
        &self,
        stream_id: &str,
        data: String,
    ) -> Result<StreamAppendOutcome, ApplicationError> {
        let Some(session) = self.active.read().await.get(stream_id).cloned() else {
            return Ok(StreamAppendOutcome::SessionClosed);
        };

        session.append(data).await
    }

    pub(super) async fn finish(&self, stream_id: &str) {
        let session = self.active.read().await.get(stream_id).cloned();
        if let Some(session) = session {
            session.finish().await;
        }
    }

    pub(super) async fn fail(&self, stream_id: &str, error: ApplicationError) {
        let Some(session) = self.active.read().await.get(stream_id).cloned() else {
            return;
        };
        let details = match &error {
            ApplicationError::UpstreamFailure(failure) => Some(failure.clone()),
            _ => None,
        };
        session.fail(error.to_string(), details).await;
    }

    pub(super) async fn close(&self, stream_id: &str) -> bool {
        let session = self.active.write().await.remove(stream_id);
        let Some(session) = session else {
            return false;
        };

        session.close().await;
        true
    }
}

struct StreamSession {
    retention: StreamRetention,
    state: Mutex<StreamSessionState>,
    changed: Notify,
    cancel: watch::Sender<bool>,
}

impl StreamSession {
    fn new(retention: StreamRetention) -> (Self, watch::Receiver<bool>) {
        let (cancel, receiver) = watch::channel(false);
        (
            Self {
                retention,
                state: Mutex::new(StreamSessionState {
                    // Count the retained JSON array delimiters as well as its frames.
                    buffered_bytes: if retention == StreamRetention::Replayable {
                        2
                    } else {
                        0
                    },
                    ..StreamSessionState::default()
                }),
                changed: Notify::new(),
                cancel,
            },
            receiver,
        )
    }

    fn event_bytes(&self, event: &ChatCompletionStreamEventDto) -> usize {
        match self.retention {
            StreamRetention::Acknowledged => event.buffered_bytes(),
            // Include each event's separator; the first separator is conservative headroom.
            StreamRetention::Replayable => {
                serde_json::to_vec(event)
                    .expect("stream event serialization is infallible")
                    .len()
                    + 1
            }
        }
    }

    fn push_terminal(&self, state: &mut StreamSessionState, event: ChatCompletionStreamEventDto) {
        state.buffered_bytes += self.event_bytes(&event);
        state.events.push_back(event);
    }

    fn wake_readers(&self) {
        // A lost HTTP response can leave the old cursor read pending while its retry arrives.
        // Register each waiter before checking state and wake all of them, including at terminal.
        self.changed.notify_waiters();
    }

    async fn append(&self, data: String) -> Result<StreamAppendOutcome, ApplicationError> {
        let mut state = self.state.lock().await;
        if state.status != ChatCompletionStreamStatusDto::Running {
            return Ok(StreamAppendOutcome::SessionClosed);
        }
        let provider_done = data == PROVIDER_DONE_SENTINEL;
        let event = ChatCompletionStreamEventDto::Chunk {
            seq: state.next_seq,
            data,
        };
        let bytes = self.event_bytes(&event);
        let limit = if self.retention == StreamRetention::Replayable {
            MAX_REPLAY_BYTES - TERMINAL_RESERVE_BYTES
        } else {
            MAX_REPLAY_BYTES
        };
        if state.buffered_bytes.saturating_add(bytes) > limit
            || (self.retention == StreamRetention::Replayable
                && state.events.len() + 2 > MAX_REPLAY_FRAMES)
        {
            return Err(ApplicationError::InternalError(format!(
                "Chat completion stream replay buffer exceeded {} MiB or {MAX_REPLAY_FRAMES} frames",
                MAX_REPLAY_BYTES / 1024 / 1024
            )));
        }

        state.next_seq += 1;
        state.buffered_bytes += bytes;
        state.events.push_back(event);

        if provider_done {
            let seq = state.next_seq;
            state.next_seq += 1;
            self.push_terminal(&mut state, ChatCompletionStreamEventDto::Done { seq });
            state.status = ChatCompletionStreamStatusDto::Done;
        }

        drop(state);
        self.wake_readers();
        Ok(if provider_done {
            StreamAppendOutcome::ProviderDone
        } else {
            StreamAppendOutcome::Appended
        })
    }

    async fn finish(&self) {
        let mut state = self.state.lock().await;
        if state.status != ChatCompletionStreamStatusDto::Running {
            return;
        }

        let seq = state.next_seq;
        state.next_seq += 1;
        self.push_terminal(&mut state, ChatCompletionStreamEventDto::Done { seq });
        state.status = ChatCompletionStreamStatusDto::Done;
        drop(state);
        self.wake_readers();
    }

    async fn fail(&self, message: String, details: Option<UpstreamFailure>) {
        let mut state = self.state.lock().await;
        if state.status != ChatCompletionStreamStatusDto::Running {
            return;
        }

        let seq = state.next_seq;
        state.next_seq += 1;
        let mut event = ChatCompletionStreamEventDto::Error {
            seq,
            message,
            details,
        };
        if self.retention == StreamRetention::Replayable
            && self.event_bytes(&event) > MAX_REPLAY_BYTES - state.buffered_bytes
        {
            event = ChatCompletionStreamEventDto::Error {
                seq,
                message: "Chat completion error exceeded the retained wire-history limit".into(),
                details: None,
            };
        }
        self.push_terminal(&mut state, event);
        state.status = ChatCompletionStreamStatusDto::Error;
        drop(state);
        self.wake_readers();
    }

    async fn close(&self) {
        let mut state = self.state.lock().await;
        let was_running = state.status == ChatCompletionStreamStatusDto::Running;
        if was_running {
            state.status = ChatCompletionStreamStatusDto::Cancelled;
        }
        drop(state);

        if was_running {
            let _ = self.cancel.send(true);
            self.wake_readers();
        }
    }

    async fn read(
        &self,
        after_seq: u64,
    ) -> Result<ChatCompletionStreamReadResultDto, ApplicationError> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut state = self.state.lock().await;

            if self.retention == StreamRetention::Acknowledged && after_seq < state.acknowledged_seq
            {
                return Err(ApplicationError::Conflict(format!(
                    "Chat completion stream cursor {after_seq} is behind acknowledged sequence {}",
                    state.acknowledged_seq
                )));
            }
            if after_seq > state.delivered_seq {
                return Err(ApplicationError::ValidationError(format!(
                    "Chat completion stream cursor {after_seq} exceeds delivered sequence {}",
                    state.delivered_seq
                )));
            }

            if self.retention == StreamRetention::Acknowledged {
                while state
                    .events
                    .front()
                    .is_some_and(|event| event.seq() <= after_seq)
                {
                    if let Some(event) = state.events.pop_front() {
                        state.buffered_bytes = state
                            .buffered_bytes
                            .saturating_sub(self.event_bytes(&event));
                    }
                }
                state.acknowledged_seq = after_seq;
            }

            let events = state
                .events
                .iter()
                .filter(|event| event.seq() > after_seq)
                .take(READ_BATCH_SIZE)
                .cloned()
                .collect::<Vec<_>>();
            if let Some(event) = events.last() {
                state.delivered_seq = state.delivered_seq.max(event.seq());
            }

            if !events.is_empty() || state.status != ChatCompletionStreamStatusDto::Running {
                return Ok(ChatCompletionStreamReadResultDto {
                    events,
                    status: state.status,
                });
            }

            drop(state);
            notified.await;
        }
    }
}

struct StreamSessionState {
    events: VecDeque<ChatCompletionStreamEventDto>,
    status: ChatCompletionStreamStatusDto,
    next_seq: u64,
    delivered_seq: u64,
    acknowledged_seq: u64,
    buffered_bytes: usize,
}

impl Default for StreamSessionState {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            status: ChatCompletionStreamStatusDto::Running,
            next_seq: 1,
            delivered_seq: 0,
            acknowledged_seq: 0,
            buffered_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Wake, Waker},
    };

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    use super::*;

    #[tokio::test]
    async fn unacknowledged_stream_events_are_replayed_in_sequence() {
        let (session, _) = StreamSession::new(StreamRetention::Acknowledged);
        let wakes = [
            Arc::new(WakeCount::default()),
            Arc::new(WakeCount::default()),
        ];
        let wakers = wakes.clone().map(Waker::from);
        let mut first_wait = Box::pin(session.read(0));
        let mut retry_wait = Box::pin(session.read(0));
        assert!(
            first_wait
                .as_mut()
                .poll(&mut Context::from_waker(&wakers[0]))
                .is_pending()
        );
        assert!(
            retry_wait
                .as_mut()
                .poll(&mut Context::from_waker(&wakers[1]))
                .is_pending()
        );
        session.append("one".to_string()).await.unwrap();
        assert!(wakes.iter().all(|count| count.0.load(Ordering::SeqCst) > 0));

        let first = first_wait.await.unwrap();
        assert_eq!(retry_wait.await.unwrap(), first);
        let replay = session.read(0).await.unwrap();
        assert_eq!(replay, first);

        session.append("two".to_string()).await.unwrap();
        let second = session.read(1).await.unwrap();
        assert_eq!(second.events[0].seq(), 2);

        session.finish().await;
        let terminal = session.read(2).await.unwrap();
        assert!(matches!(
            terminal.events.as_slice(),
            [ChatCompletionStreamEventDto::Done { seq: 3 }]
        ));

        let acknowledged = session.read(3).await.unwrap();
        assert!(acknowledged.events.is_empty());
        assert_eq!(acknowledged.status, ChatCompletionStreamStatusDto::Done);
    }

    #[tokio::test]
    async fn replayable_history_survives_reads_and_rejects_future_cursors() {
        let (session, _) = StreamSession::new(StreamRetention::Replayable);
        for index in 0..READ_BATCH_SIZE + 2 {
            session.append(index.to_string()).await.unwrap();
        }
        session.finish().await;
        // Even buffered/terminal events cannot be acknowledged before they have been delivered.
        assert!(matches!(
            session.read(1).await,
            Err(ApplicationError::ValidationError(_))
        ));
        let first = session.read(0).await.unwrap();
        assert_eq!(first.events.len(), READ_BATCH_SIZE);
        assert_eq!(first.status, ChatCompletionStreamStatusDto::Done);
        assert!(matches!(
            session.read(READ_BATCH_SIZE as u64 + 1).await,
            Err(ApplicationError::ValidationError(_))
        ));
        let tail = session.read(READ_BATCH_SIZE as u64).await.unwrap();
        assert_eq!(tail.events.len(), 3);
        assert!(matches!(
            tail.events.last(),
            Some(ChatCompletionStreamEventDto::Done { .. })
        ));
        assert_eq!(session.read(0).await.unwrap(), first);
        assert_eq!(session.read(READ_BATCH_SIZE as u64).await.unwrap(), tail);
        assert!(
            session
                .read(tail.events.last().unwrap().seq())
                .await
                .unwrap()
                .events
                .is_empty()
        );

        let (native, _) = StreamSession::new(StreamRetention::Acknowledged);
        native.append("one".into()).await.unwrap();
        native.finish().await;
        native.read(0).await.unwrap();
        native.read(1).await.unwrap();
        assert!(matches!(
            native.read(0).await,
            Err(ApplicationError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn replayable_retention_bounds_wire_bytes_and_frames_including_terminal() {
        let (session, _) = StreamSession::new(StreamRetention::Replayable);
        // Escaped JSON occupies more bytes than its raw string data.
        let error = session
            .append("\\".repeat(MAX_REPLAY_BYTES / 2))
            .await
            .unwrap_err();
        session.fail(error.to_string(), None).await;
        let read = session.read(0).await.unwrap();
        assert_eq!(read.status, ChatCompletionStreamStatusDto::Error);
        assert!(serde_json::to_vec(&read.events).unwrap().len() <= MAX_REPLAY_BYTES);

        let (session, _) = StreamSession::new(StreamRetention::Replayable);
        let empty = ChatCompletionStreamEventDto::Chunk {
            seq: 1,
            data: String::new(),
        };
        let data_bytes =
            MAX_REPLAY_BYTES - TERMINAL_RESERVE_BYTES - 2 - session.event_bytes(&empty);
        session.append("x".repeat(data_bytes)).await.unwrap();
        session
            .fail("\\".repeat(TERMINAL_RESERVE_BYTES), None)
            .await;
        let read = session.read(0).await.unwrap();
        assert!(matches!(
            read.events.last(),
            Some(ChatCompletionStreamEventDto::Error { details: None, .. })
        ));
        assert!(serde_json::to_vec(&read.events).unwrap().len() <= MAX_REPLAY_BYTES);

        let (session, _) = StreamSession::new(StreamRetention::Replayable);
        for _ in 0..MAX_REPLAY_FRAMES - 1 {
            session.append("x".into()).await.unwrap();
        }
        let error = session.append("overflow".into()).await.unwrap_err();
        session.fail(error.to_string(), None).await;
        let mut history = Vec::new();
        let mut cursor = 0;
        loop {
            let read = session.read(cursor).await.unwrap();
            assert_eq!(read.status, ChatCompletionStreamStatusDto::Error);
            let Some(last) = read.events.last() else {
                break;
            };
            cursor = last.seq();
            history.extend(read.events);
        }
        assert_eq!(history.len(), MAX_REPLAY_FRAMES);
        assert!(matches!(
            history.last(),
            Some(ChatCompletionStreamEventDto::Error { .. })
        ));
        assert!(serde_json::to_vec(&history).unwrap().len() <= MAX_REPLAY_BYTES);
    }

    #[tokio::test]
    async fn provider_done_is_terminal_before_the_session_can_close() {
        let (session, cancel) = StreamSession::new(StreamRetention::Acknowledged);

        assert_eq!(
            session
                .append(PROVIDER_DONE_SENTINEL.to_string())
                .await
                .unwrap(),
            StreamAppendOutcome::ProviderDone
        );
        session.close().await;

        assert!(!*cancel.borrow());
        let terminal = session.read(0).await.unwrap();
        assert!(matches!(
            terminal.events.as_slice(),
            [
                ChatCompletionStreamEventDto::Chunk { seq: 1, data },
                ChatCompletionStreamEventDto::Done { seq: 2 },
            ] if data == PROVIDER_DONE_SENTINEL
        ));
        assert_eq!(terminal.status, ChatCompletionStreamStatusDto::Done);
    }

    #[tokio::test]
    async fn closing_a_running_session_signals_cancellation() {
        let (session, cancel) = StreamSession::new(StreamRetention::Acknowledged);

        session.close().await;

        assert!(*cancel.borrow());

        let registry = StreamSessionRegistry::default();
        let host_cancel = CancellationToken::new();
        let lock = registry.active.write().await;
        let mut registering =
            Box::pin(registry.register("late-start", StreamRetention::Replayable, &host_cancel));
        assert!(
            registering
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        host_cancel.cancel();
        drop(lock);
        assert!(matches!(
            registering.await,
            Err(ApplicationError::Cancelled(_))
        ));
        assert!(matches!(
            registry.status("late-start").await,
            Err(ApplicationError::NotFound(_))
        ));
    }
}
