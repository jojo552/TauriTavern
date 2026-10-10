//! Per-page server push: the SSE stream each browser page keeps open.
//!
//! The browser transport (`src/web/transport.js`) expects JSON frames on the default SSE message
//! event: `ready` first, then `channel` frames with a per-channel `sequence` starting at 0 and
//! strictly increasing, and `channel-end` when the server will send no more frames on a channel.
//! A channel corresponds to a Tauri `Channel` argument in the native host: the page serializes it
//! as `{"$channel": "<id>"}` and keeps it alive independently of the invoke that created it.
//!
//! A mobile browser drops the stream when it backgrounds the tab; a proxy drops it on a network
//! change. The page is still the same page, so the stream is resumable with the standard SSE
//! mechanism: data frames carry a page-wide `id`, the browser's own reconnect sends it back as
//! `Last-Event-ID`, and the bus replays what the page has not received. `ready` and reset controls
//! have no id. A page past its grace period or outside the replay window needs a new session.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::error::CommandError;

/// Bounds the replay ring's serialized JSON, not upstream Values or serialization's temporary
/// allocations. Keeping a moving window lets an online page send indefinitely.
const MAX_RETAINED_FRAMES: usize = 4096;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const MAX_CHANNEL_ID_LEN: usize = 64;

#[derive(Default)]
struct Retained {
    /// Recent data frames, oldest first; `(seq, json)`.
    frames: VecDeque<(u64, String)>,
    bytes: usize,
    next_seq: u64,
    /// A single frame exceeded the replay budget, so no stream may continue this page.
    failed: bool,
}

impl Retained {
    fn floor(&self) -> u64 {
        self.frames.front().map_or(self.next_seq, |(seq, _)| *seq)
    }
}

#[derive(Default)]
pub struct PageBus {
    retained: Mutex<Retained>,
    wake: Notify,
    channel_ids: Mutex<HashSet<String>>,
    /// Each attach supersedes the previous stream of the same page.
    attachment: AtomicU64,
    closed: CancellationToken,
}

/// Why a page could not resume its stream.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeError {
    Closed,
    /// Frames the page has not seen were discarded; it must reload.
    Gap,
}

impl PageBus {
    /// Starts (or resumes) the page's stream after `last_seen`, the last `seq` the browser received
    /// (`None` on first connect). The next cursor must be within `[floor, next_seq]`.
    pub fn attach(self: &Arc<Self>, last_seen: Option<u64>) -> Result<PageStream, ResumeError> {
        let start = match last_seen {
            None => 0,
            Some(seq) => seq.checked_add(1).ok_or(ResumeError::Gap)?,
        };
        let retained = self.retained.lock().expect("page bus poisoned");
        if retained.failed {
            return Err(ResumeError::Gap);
        }
        if self.closed.is_cancelled() {
            return Err(ResumeError::Closed);
        }
        if start < retained.floor() || start > retained.next_seq {
            return Err(ResumeError::Gap);
        }
        let attachment = self.attachment.fetch_add(1, Ordering::SeqCst) + 1;
        drop(retained);
        // Wake a superseded stream so it notices and ends.
        self.wake.notify_waiters();
        Ok(PageStream {
            bus: self.clone(),
            attachment,
            next: start,
        })
    }

    /// Cancelled when the page is closed for good; long-running pushes stop with it.
    pub fn closed(&self) -> CancellationToken {
        self.closed.clone()
    }

    pub fn close(&self) {
        let mut retained = self.retained.lock().expect("page bus poisoned");
        self.closed.cancel();
        retained.frames.clear();
        retained.bytes = 0;
        drop(retained);
        self.wake.notify_waiters();
    }

    /// Delivers the existing native event wire shape to this page only.
    pub fn emit(&self, event: &str, payload: impl Serialize) -> Result<(), ChannelClosed> {
        let payload = serde_json::to_value(payload).map_err(|_| ChannelClosed)?;
        self.push(json!({ "type": "event", "event": event, "payload": payload }))
    }

    /// Force the existing SSE gap/reset protocol when a producer lost records.
    pub fn fail_gap(&self) {
        let mut retained = self.retained.lock().expect("page bus poisoned");
        retained.failed = true;
        retained.frames.clear();
        retained.bytes = 0;
        self.closed.cancel();
        drop(retained);
        self.wake.notify_waiters();
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
            state: Mutex::new(ChannelState::default()),
        })
    }

    /// Queues a frame without waiting for the page. Only the oldest replay frames are evicted;
    /// attach and online readers explicitly detect a cursor that falls behind that window.
    fn push(&self, frame: Value) -> Result<(), ChannelClosed> {
        if self.closed.is_cancelled() {
            return Err(ChannelClosed);
        }
        let frame = frame.to_string();
        let mut retained = self.retained.lock().expect("page bus poisoned");
        if self.closed.is_cancelled() {
            return Err(ChannelClosed);
        }
        if frame.len() > MAX_RETAINED_BYTES {
            tracing::error!(
                bytes = frame.len(),
                "Page event exceeded the replay byte budget"
            );
            retained.failed = true;
            retained.frames.clear();
            retained.bytes = 0;
            self.closed.cancel();
            drop(retained);
            self.wake.notify_waiters();
            return Err(ChannelClosed);
        }
        while retained.frames.len() >= MAX_RETAINED_FRAMES
            || retained.bytes + frame.len() > MAX_RETAINED_BYTES
        {
            let (_, oldest) = retained.frames.pop_front().expect("nonempty replay ring");
            retained.bytes -= oldest.len();
        }
        let seq = retained.next_seq;
        retained.next_seq += 1;
        retained.bytes += frame.len();
        retained.frames.push_back((seq, frame));
        drop(retained);
        self.wake.notify_waiters();
        Ok(())
    }
}

/// One attached SSE stream. A newer attachment or page close ends it; falling behind the replay
/// floor returns an explicit gap rather than silently skipping channel frames.
pub struct PageStream {
    bus: Arc<PageBus>,
    attachment: u64,
    next: u64,
}

impl PageStream {
    /// The next `(id, frame)`, `None` when closed/superseded, or an explicit replay gap.
    pub async fn next(&mut self) -> Result<Option<(u64, String)>, ResumeError> {
        loop {
            let notified = self.bus.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let retained = self.bus.retained.lock().expect("page bus poisoned");
                if self.bus.attachment.load(Ordering::SeqCst) != self.attachment {
                    return Ok(None);
                }
                if retained.failed {
                    return Err(ResumeError::Gap);
                }
                if self.bus.closed.is_cancelled() {
                    return Ok(None);
                }
                let floor = retained.floor();
                if self.next < floor {
                    return Err(ResumeError::Gap);
                }
                // Index by the exact cursor: never skip missing frames in an online stream.
                if let Some((seq, frame)) = retained.frames.get((self.next - floor) as usize) {
                    self.next = seq + 1;
                    return Ok(Some((*seq, frame.clone())));
                }
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelClosed;

impl std::fmt::Display for ChannelClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the page closed its event stream")
    }
}

#[derive(Default)]
struct ChannelState {
    next_sequence: u64,
    ended: bool,
}

/// A channel may be shared by concurrent tasks (extension tool calls and their cancellations).
/// Its sequence number and its place in the page stream are assigned under one lock, so the page
/// always receives sequences in order and `channel-end` after every data frame.
pub struct PageChannel {
    id: String,
    bus: Arc<PageBus>,
    state: Mutex<ChannelState>,
}

impl PageChannel {
    pub fn send(&self, payload: impl Serialize) -> Result<(), ChannelClosed> {
        let payload = serde_json::to_value(payload).map_err(|error| {
            tracing::error!(%error, channel = %self.id, "Failed to serialize a page channel payload");
            ChannelClosed
        })?;
        let mut state = self.state.lock().expect("page channel poisoned");
        if state.ended {
            return Err(ChannelClosed);
        }
        self.bus.push(json!({ "type": "channel", "channelId": self.id, "sequence": state.next_sequence, "payload": payload }))?;
        state.next_sequence += 1;
        Ok(())
    }

    pub fn closed(&self) -> CancellationToken {
        self.bus.closed()
    }

    /// Tells the page this channel is finished; later sends fail.
    pub fn end(&self) {
        let mut state = self.state.lock().expect("page channel poisoned");
        if !state.ended {
            state.ended = true;
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

    /// Read a known number of frames; no elapsed-time guess about when a stream is drained.
    async fn frames(stream: &mut PageStream, count: usize) -> Vec<(u64, Value)> {
        let mut out = Vec::new();
        for _ in 0..count {
            let (id, frame) = stream.next().await.unwrap().expect("expected frame");
            out.push((id, serde_json::from_str(&frame).unwrap()));
        }
        out
    }

    #[tokio::test]
    async fn channel_frames_are_sequenced_and_end_once() {
        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "7" } }), "channel")
            .unwrap();
        channel.send(json!({ "a": 1 })).unwrap();
        channel.send(json!({ "a": 2 })).unwrap();
        drop(channel);
        let frames = frames(&mut stream, 3).await;
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].1["sequence"], 0);
        assert_eq!(frames[1].1["sequence"], 1);
        assert_eq!(frames[2].1["type"], "channel-end");
        assert_eq!(
            frames.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// Extension tools share one channel across concurrent calls. Whatever the interleaving, the
    /// page stream must carry that channel's sequences in order, or the page fails its transport.
    #[test]
    fn concurrent_senders_keep_channel_sequences_in_stream_order() {
        const SENDERS: usize = 8;
        const PER_SENDER: usize = 200;
        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
        let channel = Arc::new(
            bus.channel(&json!({ "channel": { "$channel": "tools" } }), "channel")
                .unwrap(),
        );
        let start = Arc::new(std::sync::Barrier::new(SENDERS));
        let senders: Vec<_> = (0..SENDERS)
            .map(|_| {
                let (channel, start) = (channel.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..PER_SENDER {
                        channel.send(json!(null)).unwrap();
                    }
                })
            })
            .collect();
        for sender in senders {
            sender.join().unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let sequences: Vec<u64> = runtime
            .block_on(frames(&mut stream, SENDERS * PER_SENDER))
            .iter()
            .map(|(_, frame)| frame["sequence"].as_u64().unwrap())
            .collect();
        assert_eq!(
            sequences,
            (0..(SENDERS * PER_SENDER) as u64).collect::<Vec<_>>()
        );
    }

    /// A dropped connection resumes from the page's last received frame: nothing it saw is
    /// repeated, nothing it missed is lost, and the superseded stream ends.
    #[tokio::test]
    async fn a_reconnecting_page_resumes_after_its_last_seen_frame() {
        let bus = Arc::new(PageBus::default());
        let mut first = bus.attach(None).unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "c" } }), "channel")
            .unwrap();
        channel.send(json!("a")).unwrap();
        channel.send(json!("b")).unwrap();
        let seen = frames(&mut first, 2).await;
        assert_eq!(seen.len(), 2);
        // The browser received only the first frame before its connection dropped.
        channel.send(json!("c")).unwrap();
        let mut second = bus.attach(Some(0)).unwrap();
        assert_eq!(first.next().await, Ok(None), "the superseded stream ends");
        let replay = frames(&mut second, 2).await;
        assert_eq!(
            replay
                .iter()
                .map(|(_, f)| f["payload"].clone())
                .collect::<Vec<_>>(),
            vec![json!("b"), json!("c")]
        );
        // A page that claims frames the bus never sent cannot resume.
        assert_eq!(bus.attach(Some(99)).err(), Some(ResumeError::Gap));
    }

    #[tokio::test]
    async fn an_online_page_keeps_sending_beyond_the_replay_frame_limit() {
        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "c" } }), "channel")
            .unwrap();
        for i in 0..=MAX_RETAINED_FRAMES {
            channel.send(i).unwrap();
            let frame = frames(&mut stream, 1).await.pop().unwrap();
            assert_eq!(frame.0, i as u64);
            assert_eq!(frame.1["payload"], json!(i));
        }
        // Only the oldest cursor is unavailable; this is not a permanently poisoned page bus.
        assert_eq!(bus.attach(None).err(), Some(ResumeError::Gap));
        let mut resumed = bus.attach(Some(0)).unwrap();
        let replay = frames(&mut resumed, MAX_RETAINED_FRAMES).await;
        assert_eq!(replay.first().unwrap().0, 1);
        assert_eq!(replay.last().unwrap().0, MAX_RETAINED_FRAMES as u64);
        channel.send("still online").unwrap();
        assert_eq!(
            frames(&mut resumed, 1).await[0].1["payload"],
            "still online"
        );
        assert_eq!(bus.attach(Some(u64::MAX)).err(), Some(ResumeError::Gap));
    }

    #[tokio::test]
    async fn a_slow_online_reader_reports_a_gap_without_skipping_frames() {
        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "c" } }), "channel")
            .unwrap();
        for i in 0..=MAX_RETAINED_FRAMES {
            channel.send(i).unwrap();
        }
        assert_eq!(stream.next().await, Err(ResumeError::Gap));
        assert_eq!(bus.attach(None).err(), Some(ResumeError::Gap));
        let mut resumed = bus.attach(Some(0)).unwrap();
        assert_eq!(frames(&mut resumed, 1).await[0].0, 1);
    }

    #[tokio::test]
    async fn replay_byte_budget_evicts_oldest_frames_and_oversize_closes_the_page() {
        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "c" } }), "channel")
            .unwrap();
        let payload = "x".repeat(MAX_RETAINED_BYTES / 2);
        channel.send(&payload).unwrap();
        let first = frames(&mut stream, 1).await.pop().unwrap();
        assert_eq!(first.0, 0);
        assert_eq!(first.1["payload"], payload);
        channel.send(&payload).unwrap();
        assert_eq!(bus.attach(None).err(), Some(ResumeError::Gap));
        let mut resumed = bus.attach(Some(0)).unwrap();
        assert_eq!(frames(&mut resumed, 1).await[0].0, 1);
        assert_eq!(
            channel.send("x".repeat(MAX_RETAINED_BYTES)),
            Err(ChannelClosed)
        );
        assert!(channel.closed().is_cancelled());
        assert_eq!(resumed.next().await, Err(ResumeError::Gap));
        assert_eq!(bus.attach(Some(1)).err(), Some(ResumeError::Gap));
        assert_eq!(channel.send("after failure"), Err(ChannelClosed));
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
    fn a_closed_page_rejects_sends_and_resumes() {
        let bus = Arc::new(PageBus::default());
        let channel = bus
            .channel(&json!({ "channel": { "$channel": "1" } }), "channel")
            .unwrap();
        bus.close();
        assert_eq!(channel.send(json!(1)), Err(ChannelClosed));
        assert_eq!(bus.attach(None).err(), Some(ResumeError::Closed));
    }

    /// The shared extension tool bridge delivers a call to the registering page's stream, accepts
    /// that page's reply, and refuses replies from any other page.
    #[tokio::test]
    async fn extension_tool_calls_round_trip_through_the_page_stream() {
        use tt_adapter_extension::tools::AgentExtensionTools;
        use tt_contracts::extension_tools::{
            ExtensionToolCall, ExtensionToolDefinition, ExtensionToolReply, ExtensionToolTarget,
        };
        use tt_domain::models::tool::AgentToolScope;
        use tt_ports::extension_tools::ExtensionTools;

        let bus = Arc::new(PageBus::default());
        let mut stream = bus.attach(None).unwrap();
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
        let frame = frames(&mut stream, 1).await.pop().unwrap().1;
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
