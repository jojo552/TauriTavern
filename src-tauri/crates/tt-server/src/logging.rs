//! Collection belongs to the shared adapter; this host owns only per-page delivery.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::events::PageBus;

#[derive(Default)]
pub struct PageLogStreams {
    pub backend: LogSubscription,
    pub llm: LogSubscription,
}

#[derive(Default)]
pub struct LogSubscription {
    task: Mutex<Option<JoinHandle<()>>>,
}

impl LogSubscription {
    /// Enabling twice does not replace a live receiver. Disabling joins the old pump before
    /// returning: no frame from that subscription can appear after its confirmation.
    pub async fn set<T>(
        &self,
        enabled: bool,
        subscribe: impl FnOnce() -> broadcast::Receiver<T>,
        bus: Arc<PageBus>,
        event: &'static str,
    ) where
        T: Clone + Serialize + Send + 'static,
    {
        let mut slot = self.task.lock().await;
        if enabled && slot.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        if let Some(task) = slot.take() {
            task.abort();
            let _ = task.await;
        }
        if enabled && !bus.closed().is_cancelled() {
            *slot = Some(tokio::spawn(pump(subscribe(), bus, event)));
        }
    }
}

impl Drop for LogSubscription {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}

async fn pump<T>(mut receiver: broadcast::Receiver<T>, bus: Arc<PageBus>, event: &'static str)
where
    T: Clone + Serialize + Send + 'static,
{
    let closed = bus.closed();
    loop {
        let entry = tokio::select! {
            biased;
            _ = closed.cancelled() => return,
            entry = receiver.recv() => entry,
        };
        match entry {
            Ok(entry) => {
                // Never trace delivery: doing so would generate another backend log frame.
                if bus.emit(event, entry).is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Reuse SSE's gap/reset protocol rather than silently skipping lost records.
                bus.fail_gap();
                return;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ResumeError;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn subscriptions_are_page_scoped_and_disable_stops_only_its_own_pump() {
        let (sender, _) = broadcast::channel::<Value>(8);
        let a = Arc::new(PageBus::default());
        let b = Arc::new(PageBus::default());
        let mut stream_a = a.attach(None).unwrap();
        let mut stream_b = b.attach(None).unwrap();
        let logs_a = PageLogStreams::default();
        let logs_b = PageLogStreams::default();
        logs_a
            .backend
            .set(
                true,
                || sender.subscribe(),
                a.clone(),
                "tauritavern-backend-log",
            )
            .await;
        logs_b
            .backend
            .set(
                true,
                || sender.subscribe(),
                b.clone(),
                "tauritavern-backend-log",
            )
            .await;
        logs_a
            .backend
            .set(
                true,
                || -> broadcast::Receiver<Value> {
                    panic!("enabling again must keep the receiver")
                },
                a.clone(),
                "tauritavern-backend-log",
            )
            .await;
        sender.send(json!({"id": 1})).unwrap();
        for stream in [&mut stream_a, &mut stream_b] {
            let (_, frame) = stream.next().await.unwrap().unwrap();
            let frame: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(frame["type"], "event");
            assert_eq!(frame["event"], "tauritavern-backend-log");
            assert_eq!(frame["payload"]["id"], 1);
        }
        logs_a
            .backend
            .set(
                false,
                || sender.subscribe(),
                a.clone(),
                "tauritavern-backend-log",
            )
            .await;
        assert_eq!(sender.receiver_count(), 1);
        sender.send(json!({"id": 2})).unwrap();
        let (_, frame) = stream_b.next().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["payload"]["id"], 2);
        let mut replay_a = a.attach(Some(0)).unwrap();
        // A's own marker is the next frame, not B's log.
        a.emit("marker", json!(null)).unwrap();
        let (_, frame) = replay_a.next().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&frame).unwrap()["event"],
            "marker"
        );
        b.close();
        logs_b
            .backend
            .set(
                false,
                || sender.subscribe(),
                b.clone(),
                "tauritavern-backend-log",
            )
            .await;
        assert_eq!(sender.receiver_count(), 0);
        assert_eq!(stream_b.next().await, Ok(None));
    }

    #[tokio::test]
    async fn a_lagged_subscription_reports_a_gap_instead_of_skipping_records() {
        let (sender, receiver) = broadcast::channel::<Value>(1);
        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
        let subscription = LogSubscription::default();
        // Fill the receiver before giving it to the pump; no scheduling guess or timer.
        sender.send(json!({"id": 1})).unwrap();
        sender.send(json!({"id": 2})).unwrap();
        subscription
            .set(true, || receiver, bus.clone(), "tauritavern-llm-api-log")
            .await;
        assert_eq!(stream.next().await, Err(ResumeError::Gap));
        subscription
            .set(
                false,
                || sender.subscribe(),
                bus.clone(),
                "tauritavern-llm-api-log",
            )
            .await;
        assert_eq!(sender.receiver_count(), 0);
        assert!(bus.closed().is_cancelled());
    }
}
