//! Per-page server push: the single SSE stream each browser page opens.
//!
//! The browser transport (`src/web/transport.js`) expects JSON frames on the default SSE message
//! event: `ready` first, then `channel` frames with a per-channel `sequence` starting at 0 and
//! strictly increasing, and `channel-end` when the server will send no more frames on a channel.
//! A channel corresponds to a Tauri `Channel` argument in the native host: the page serializes it
//! as `{"$channel": "<id>"}` and keeps it alive independently of the invoke that created it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::CommandError;

/// A page that stops reading its stream must not grow server memory without bound.
const MAX_QUEUED_FRAMES: usize = 4096;
const MAX_CHANNEL_ID_LEN: usize = 64;

pub struct PageBus {
    sender: mpsc::Sender<String>,
    receiver: Mutex<Option<mpsc::Receiver<String>>>,
    channel_ids: Mutex<HashSet<String>>,
    closed: CancellationToken,
}

impl Default for PageBus {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(MAX_QUEUED_FRAMES);
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            channel_ids: Mutex::new(HashSet::new()),
            closed: CancellationToken::new(),
        }
    }
}

impl PageBus {
    /// The page's event stream takes the receiver once; a second stream is a protocol error.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<String>> {
        self.receiver.lock().expect("page bus poisoned").take()
    }

    /// Cancelled when the page's event stream ends; long-running pushes stop with it.
    pub fn closed(&self) -> CancellationToken {
        self.closed.clone()
    }

    pub fn close(&self) {
        self.closed.cancel();
    }

    /// Resolves a `{"$channel": id}` argument. Each id may be opened once per page.
    pub fn channel(
        self: &Arc<Self>,
        args: &Value,
        name: &str,
    ) -> Result<PageChannel, CommandError> {
        let id = args
            .get(name)
            .and_then(|value| value.get("$channel"))
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= MAX_CHANNEL_ID_LEN
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| {
                CommandError::BadRequest(format!("Argument {name} must be a page channel"))
            })?
            .to_string();
        if !self
            .channel_ids
            .lock()
            .expect("page bus poisoned")
            .insert(id.clone())
        {
            return Err(CommandError::Conflict(format!(
                "Page channel {id} is already in use"
            )));
        }
        Ok(PageChannel {
            id,
            bus: self.clone(),
            next_sequence: AtomicU64::new(0),
            ended: AtomicBool::new(false),
        })
    }

    fn push(&self, frame: Value) -> Result<(), ChannelClosed> {
        // `try_send` never blocks a service on a slow page: a full queue means the page is not
        // reading, which the transport treats as a broken stream anyway.
        self.sender
            .try_send(frame.to_string())
            .map_err(|_| ChannelClosed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelClosed;

impl std::fmt::Display for ChannelClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the page closed its event stream")
    }
}

pub struct PageChannel {
    id: String,
    bus: Arc<PageBus>,
    next_sequence: AtomicU64,
    ended: AtomicBool,
}

impl PageChannel {
    pub fn send(&self, payload: impl Serialize) -> Result<(), ChannelClosed> {
        if self.ended.load(Ordering::SeqCst) {
            return Err(ChannelClosed);
        }
        let payload = serde_json::to_value(payload).map_err(|error| {
            tracing::error!(%error, channel = %self.id, "Failed to serialize a page channel payload");
            ChannelClosed
        })?;
        let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        self.bus.push(json!({ "type": "channel", "channelId": self.id, "sequence": sequence, "payload": payload }))
    }

    pub fn closed(&self) -> CancellationToken {
        self.bus.closed()
    }

    /// Tells the page this channel is finished; later sends fail.
    pub fn end(&self) {
        if !self.ended.swap(true, Ordering::SeqCst) {
            let _ = self
                .bus
                .push(json!({ "type": "channel-end", "channelId": self.id }));
        }
    }
}

impl Drop for PageChannel {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(receiver: &mut mpsc::Receiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(frame) = receiver.try_recv() {
            out.push(serde_json::from_str(&frame).unwrap());
        }
        out
    }

    #[test]
    fn channel_frames_are_sequenced_and_end_once() {
        let bus = Arc::new(PageBus::default());
        let mut receiver = bus.take_receiver().unwrap();
        assert!(bus.take_receiver().is_none());
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "7" } }), "channel")
            .unwrap();
        channel.send(json!({ "a": 1 })).unwrap();
        channel.send(json!({ "a": 2 })).unwrap();
        drop(channel);
        let frames = frames(&mut receiver);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["sequence"], 0);
        assert_eq!(frames[1]["sequence"], 1);
        assert_eq!(
            frames[2],
            json!({ "type": "channel-end", "channelId": "7" })
        );
    }

    #[test]
    fn channel_ids_are_single_use_and_validated() {
        let bus = Arc::new(PageBus::default());
        let _first = bus
            .channel(&json!({ "channel": { "$channel": "1" } }), "channel")
            .unwrap();
        assert!(
            bus.channel(&json!({ "channel": { "$channel": "1" } }), "channel")
                .is_err()
        );
        assert!(
            bus.channel(&json!({ "channel": { "$channel": "../x" } }), "channel")
                .is_err()
        );
        assert!(
            bus.channel(&json!({ "channel": "plain" }), "channel")
                .is_err()
        );
    }

    #[test]
    fn a_closed_page_rejects_sends() {
        let bus = Arc::new(PageBus::default());
        let receiver = bus.take_receiver().unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "1" } }), "channel")
            .unwrap();
        drop(receiver);
        assert_eq!(channel.send(json!(1)), Err(ChannelClosed));
    }

    /// The shared extension tool bridge delivers a call to the registering page's stream, accepts
    /// that page's reply, and refuses replies from any other page.
    #[tokio::test]
    async fn extension_tool_calls_round_trip_through_the_page_stream() {
        use tt_contracts::extension_tools::{
            ExtensionToolCall, ExtensionToolDefinition, ExtensionToolReply, ExtensionToolTarget,
        };
        use tt_domain::models::tool::AgentToolScope;
        use tt_ports::extension_tools::ExtensionTools;
        use tt_runtime::extension_tools::AgentExtensionTools;

        let bus = Arc::new(PageBus::default());
        let mut receiver = bus.take_receiver().unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "tools" } }), "channel")
            .unwrap();
        let bridge = Arc::new(AgentExtensionTools::default());
        let tool_id = bridge
            .register(
                "page-a",
                ExtensionToolDefinition {
                    extension_id: "probe".into(),
                    name: "echo".into(),
                    description: "Echo".into(),
                    input_schema: json!({ "type": "object" }),
                    contexts: vec![AgentToolScope::Session],
                    enabled: true,
                },
                Arc::new(channel),
            )
            .unwrap();
        let (_cancel, cancel) = tokio::sync::watch::channel(false);
        let call = tokio::spawn({
            let bridge = bridge.clone();
            async move {
                bridge
                    .call(
                        ExtensionToolCall {
                            tool_id,
                            run_id: "run".into(),
                            invocation_id: "inv".into(),
                            call_id: "call".into(),
                            target: ExtensionToolTarget::Session {
                                session_id: "s".into(),
                            },
                            arguments: serde_json::Map::new(),
                        },
                        cancel,
                    )
                    .await
            }
        });
        let frame: Value = serde_json::from_str(&receiver.recv().await.unwrap()).unwrap();
        assert_eq!(frame["type"], "channel");
        assert_eq!(frame["payload"]["type"], "call");
        let request_id = frame["payload"]["requestId"].as_str().unwrap().to_string();
        let reply = ExtensionToolReply::Value {
            value: json!({ "ok": true }),
        };
        assert!(!bridge.resolve("page-b", &request_id, reply.clone()));
        assert!(bridge.resolve("page-a", &request_id, reply));
        assert!(matches!(
            call.await.unwrap(),
            Ok(ExtensionToolReply::Value { .. })
        ));
    }
}
