//! The password-protected browser host is single-user: explicit user errors go to its pages.
//! Ordinary logs never enter this hub. Delivery is independent of the Dev log subscription.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tt_contracts::observability::{BACKEND_ERROR_EVENT, BackendErrorPayload};

use crate::error::CommandError;
use crate::events::PageBus;

const MAX_PENDING: usize = 50;
const MAX_MESSAGE_BYTES: usize = 2048;
const MAX_EVENTS_PER_TICK: usize = 4;
const DELIVERY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Default)]
struct PendingErrors {
    messages: VecDeque<String>,
    omitted: u64,
}

impl PendingErrors {
    fn push(&mut self, message: String) {
        if self.messages.contains(&message) {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        if self.messages.len() == MAX_PENDING {
            self.messages.pop_front();
            self.omitted = self.omitted.saturating_add(1);
        }
        self.messages.push_back(message);
    }

    fn deliver(&mut self, bus: &PageBus) {
        // A small bounded rate keeps an error storm from filling the shared SSE replay window.
        // Coalescing is visible, rather than poisoning the page and invalidating AI/file handles.
        let mut remaining = MAX_EVENTS_PER_TICK;
        if self.omitted > 0 {
            // Reserve one of the existing frontend's 50 early-message slots for this summary.
            if self.messages.len() == MAX_PENDING {
                self.messages.pop_front();
                self.omitted = self.omitted.saturating_add(1);
            }
            let message = format!(
                "At least {} additional backend errors were coalesced or omitted. See backend logs for details.",
                self.omitted
            );
            self.omitted = 0;
            if bus
                .emit(BACKEND_ERROR_EVENT, BackendErrorPayload { message })
                .is_err()
            {
                return;
            }
            remaining -= 1;
        }
        for _ in 0..remaining {
            let Some(message) = self.messages.pop_front() else {
                break;
            };
            if bus
                .emit(BACKEND_ERROR_EVENT, BackendErrorPayload { message })
                .is_err()
            {
                break;
            }
        }
    }
}

struct PageErrors {
    bus: Weak<PageBus>,
    ready: bool,
    pending: PendingErrors,
}

#[derive(Default)]
struct State {
    pages: HashMap<String, PageErrors>,
    /// Startup errors are available to pages created before the first bridge becomes ready.
    /// Once consumed, later new pages do not replay offline/history errors.
    startup: PendingErrors,
    ever_ready: bool,
}

#[derive(Default)]
pub struct BackendErrorHub {
    state: Mutex<State>,
}

impl BackendErrorHub {
    pub fn publish(&self, message: String) {
        let message = bounded_message(&message);
        if message.is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("backend error hub poisoned");
        if !state.ever_ready {
            state.startup.push(message.clone());
        }
        state.pages.retain(|_, page| {
            let Some(bus) = page.bus.upgrade() else {
                return false;
            };
            if bus.closed().is_cancelled() {
                return false;
            }
            page.pending.push(message.clone());
            true
        });
    }

    /// The registry is only a weak delivery index. Security owns page lifetime and authorization.
    pub fn register_page(&self, id: &str, bus: &Arc<PageBus>) {
        let mut state = self.state.lock().expect("backend error hub poisoned");
        let pending = if state.ever_ready {
            PendingErrors::default()
        } else {
            state.startup.clone()
        };
        state.pages.entry(id.to_string()).or_insert(PageErrors {
            bus: Arc::downgrade(bus),
            ready: false,
            pending,
        });
    }

    pub fn remove_page(&self, id: &str) {
        self.state
            .lock()
            .expect("backend error hub poisoned")
            .pages
            .remove(id);
    }

    pub fn ready(&self, id: &str) -> Result<(), CommandError> {
        let mut state = self.state.lock().expect("backend error hub poisoned");
        let page = state.pages.get_mut(id).ok_or_else(|| {
            CommandError::InternalServerError("Backend error bridge page is not registered".into())
        })?;
        let bus = page
            .bus
            .upgrade()
            .filter(|bus| !bus.closed().is_cancelled())
            .ok_or_else(|| {
                CommandError::BadRequest("Backend error bridge page is closed".into())
            })?;
        if page.ready {
            return Ok(());
        }
        // Ready, draining and live queuing share this lock. Pending is put in SSE, not the HTTP
        // response: a lost ready response can still replay the notifications on the same page.
        page.pending.deliver(&bus);
        page.ready = true;
        state.ever_ready = true;
        state.startup = PendingErrors::default();
        Ok(())
    }

    fn deliver_pending(&self) {
        let mut state = self.state.lock().expect("backend error hub poisoned");
        state.pages.retain(|_, page| {
            let Some(bus) = page.bus.upgrade() else {
                return false;
            };
            if bus.closed().is_cancelled() {
                return false;
            }
            if page.ready {
                page.pending.deliver(&bus);
            }
            true
        });
    }

    pub async fn run(&self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(DELIVERY_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => self.deliver_pending(),
            }
        }
    }
}

fn bounded_message(message: &str) -> String {
    let message = message.trim();
    if message.len() <= MAX_MESSAGE_BYTES {
        return message.to_string();
    }
    const SUFFIX: &str = "… [truncated; see backend logs]";
    let mut end = MAX_MESSAGE_BYTES - SUFFIX.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{SUFFIX}", &message[..end])
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tracing_subscriber::prelude::*;
    use tt_adapter_observability::user_errors::BackendUserErrorLayer;
    use tt_contracts::observability::USER_VISIBLE_ERROR;

    use super::*;

    async fn message(stream: &mut crate::events::PageStream) -> (u64, String) {
        let (cursor, frame) = stream.next().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["event"], BACKEND_ERROR_EVENT);
        assert!(frame["id"].is_u64());
        assert_eq!(frame["payload"].as_object().unwrap().len(), 1);
        (
            cursor,
            frame["payload"]["message"].as_str().unwrap().to_string(),
        )
    }

    #[tokio::test]
    async fn explicit_errors_cross_ready_and_replay_once_without_a_log_subscription() {
        let hub = Arc::new(BackendErrorHub::default());
        let captured = hub.clone();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(
            BackendUserErrorLayer::new(Arc::new(move |message| captured.publish(message))),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!(target: "ordinary", "not a toast");
            tracing::warn!(target: USER_VISIBLE_ERROR, "not an error");
            tracing::error!(target: USER_VISIBLE_ERROR, private_field = "not part of payload", "startup");
        });
        let bus = Arc::new(PageBus::default());
        hub.register_page("page", &bus);
        let mut stream = bus.attach(None).unwrap();
        hub.ready("page").unwrap();
        hub.ready("page").unwrap();
        assert_eq!(message(&mut stream).await, (0, "startup".into()));
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!(target: USER_VISIBLE_ERROR, "live");
        });
        hub.deliver_pending();
        drop(stream);
        let mut resumed = bus.attach(Some(0)).unwrap();
        assert_eq!(message(&mut resumed).await, (1, "live".into()));
        bus.close();
        assert!(resumed.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_ready_and_publish_deliver_each_pending_error_once() {
        let hub = BackendErrorHub::default();
        let bus = Arc::new(PageBus::default());
        hub.register_page("page", &bus);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            threads.spawn(|| {
                barrier.wait();
                hub.ready("page").unwrap();
            });
            threads.spawn(|| {
                barrier.wait();
                for index in 0..40 {
                    hub.publish(format!("error {index:02}"));
                }
            });
        });
        for _ in 0..10 {
            hub.deliver_pending();
        }
        hub.ready("page").unwrap();
        let mut stream = bus.attach(None).unwrap();
        for index in 0..40 {
            assert_eq!(
                message(&mut stream).await,
                (index, format!("error {index:02}"))
            );
        }
        bus.close();
        assert!(stream.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn env_filter_applies_to_notifications_as_well_as_logs() {
        let hub = Arc::new(BackendErrorHub::default());
        let captured = hub.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "info,tauritavern::user_error=off",
            ))
            .with(BackendUserErrorLayer::new(Arc::new(move |message| {
                captured.publish(message)
            })));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: USER_VISIBLE_ERROR, "filtered out");
        });
        let bus = Arc::new(PageBus::default());
        hub.register_page("page", &bus);
        hub.ready("page").unwrap();
        hub.publish("unfiltered hub message".into());
        hub.deliver_pending();
        let mut stream = bus.attach(None).unwrap();
        assert_eq!(
            message(&mut stream).await,
            (0, "unfiltered hub message".into())
        );
    }

    #[tokio::test]
    async fn each_page_declares_ready_and_closed_pages_do_not_receive_or_replay_history() {
        let hub = BackendErrorHub::default();
        let a = Arc::new(PageBus::default());
        let b = Arc::new(PageBus::default());
        hub.register_page("a", &a);
        hub.register_page("b", &b);
        hub.ready("a").unwrap();
        hub.publish("shared single-user error".into());
        hub.deliver_pending();
        let mut stream_a = a.attach(None).unwrap();
        assert_eq!(message(&mut stream_a).await.1, "shared single-user error");
        hub.ready("b").unwrap();
        let mut stream_b = b.attach(None).unwrap();
        assert_eq!(message(&mut stream_b).await.1, "shared single-user error");
        a.close();
        hub.remove_page("a");
        assert!(hub.ready("a").is_err());
        hub.publish("second".into());
        hub.deliver_pending();
        assert_eq!(message(&mut stream_b).await.1, "second");
        let fresh = Arc::new(PageBus::default());
        hub.register_page("fresh", &fresh);
        hub.ready("fresh").unwrap();
        hub.publish("new page live".into());
        hub.deliver_pending();
        let mut fresh_stream = fresh.attach(None).unwrap();
        assert_eq!(
            message(&mut fresh_stream).await,
            (0, "new page live".into())
        );
    }

    #[tokio::test]
    async fn storm_is_bounded_visible_and_does_not_reset_other_page_resources() {
        let hub = BackendErrorHub::default();
        let bus = Arc::new(PageBus::default());
        hub.register_page("page", &bus);
        hub.ready("page").unwrap();
        for _ in 0..10000 {
            hub.publish("duplicate".into());
        }
        for index in 0..100 {
            hub.publish(format!("distinct {index}"));
        }
        hub.deliver_pending();
        let mut stream = bus.attach(None).unwrap();
        assert!(
            message(&mut stream)
                .await
                .1
                .contains("coalesced or omitted")
        );
        for index in 51..54 {
            assert_eq!(message(&mut stream).await.1, format!("distinct {index}"));
        }
        let channel = bus
            .channel(
                &serde_json::json!({ "channel": { "$channel": "still-live" } }),
                "channel",
            )
            .unwrap();
        channel.send("other resources still work").unwrap();
        let (_, frame) = stream.next().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&frame).unwrap()["type"],
            "channel"
        );
        assert!(!bus.closed().is_cancelled());
        for _ in 0..12 {
            hub.deliver_pending();
        }
        for index in 54..100 {
            assert_eq!(message(&mut stream).await.1, format!("distinct {index}"));
        }
        // The whole storm drain was one summary + 49 messages: it fits the frontend early queue.
        hub.publish("中文".repeat(MAX_MESSAGE_BYTES));
        hub.deliver_pending();
        let last = message(&mut stream).await.1;
        assert!(last.len() <= MAX_MESSAGE_BYTES);
        assert!(last.ends_with("[truncated; see backend logs]"));
    }
}
