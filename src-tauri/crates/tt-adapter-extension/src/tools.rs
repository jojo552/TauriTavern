//! Page-owned agent extension tools, shared by every host.
//!
//! A page registers tools and receives calls through an [`ExtensionToolEventSink`] supplied by its
//! host. The owner is an opaque page identity (a WebView label, a browser page session) used to
//! keep one page from resolving or disabling another page's tools.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{oneshot, watch};
use tt_contracts::extension_tools::{
    ExtensionTool, ExtensionToolCall, ExtensionToolDefinition, ExtensionToolEvent,
    ExtensionToolReply,
};
use tt_domain::{errors::DomainError, models::tool::ToolId};
use tt_ports::extension_tools::{ExtensionToolEventSink, ExtensionTools};

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

struct Registration {
    tool: ExtensionTool,
    sink: Arc<dyn ExtensionToolEventSink>,
    owner: String,
}

struct PendingCall {
    owner: String,
    sender: oneshot::Sender<ExtensionToolReply>,
}

#[derive(Default)]
struct BridgeState {
    tools: HashMap<ToolId, Registration>,
    pending: HashMap<String, PendingCall>,
}

/// Page-owned callbacks and their outstanding receipts. Tool policy stays in application.
#[derive(Default)]
pub struct AgentExtensionTools {
    state: Mutex<BridgeState>,
}

impl AgentExtensionTools {
    pub fn register(
        &self,
        owner: &str,
        definition: ExtensionToolDefinition,
        sink: Arc<dyn ExtensionToolEventSink>,
    ) -> Result<ToolId, DomainError> {
        let tool = definition.into_tool()?;
        let id = tool.descriptor.id.clone();
        let mut state = self.state.lock().expect("extension tool bridge poisoned");
        match state.tools.entry(id.clone()) {
            Entry::Occupied(_) => Err(DomainError::Conflict(format!(
                "extension.tool_already_registered: `{id}` is already registered"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(Registration {
                    tool,
                    sink,
                    owner: owner.to_owned(),
                });
                Ok(id)
            }
        }
    }

    pub fn set_enabled(
        &self,
        owner: &str,
        tool_id: &ToolId,
        enabled: bool,
    ) -> Result<(), DomainError> {
        let mut state = self.state.lock().expect("extension tool bridge poisoned");
        let registration = state
            .tools
            .get_mut(tool_id)
            .filter(|registration| registration.owner == owner)
            .ok_or_else(|| {
                DomainError::NotFound(format!(
                    "extension.tool_not_registered: `{tool_id}` is not registered by this page"
                ))
            })?;
        registration.tool.enabled = enabled;
        Ok(())
    }

    pub fn resolve(&self, owner: &str, request_id: &str, result: ExtensionToolReply) -> bool {
        let mut state = self.state.lock().expect("extension tool bridge poisoned");
        if !state
            .pending
            .get(request_id)
            .is_some_and(|pending| pending.owner == owner)
        {
            return false;
        }
        state
            .pending
            .remove(request_id)
            .expect("pending call checked under the same lock")
            .sender
            .send(result)
            .is_ok()
    }

    pub fn clear_page(&self, owner: &str) {
        let removed = {
            let mut state = self.state.lock().expect("extension tool bridge poisoned");
            state.pending.retain(|_, pending| pending.owner != owner);
            state
                .tools
                .extract_if(|_, registration| registration.owner == owner)
                .collect::<Vec<_>>()
        };
        // Dropping a sink can dispatch to the page; never do it under the lock.
        drop(removed);
    }
}

#[async_trait]
impl ExtensionTools for AgentExtensionTools {
    fn list(&self) -> Result<Vec<ExtensionTool>, DomainError> {
        let state = self.state.lock().expect("extension tool bridge poisoned");
        Ok(state
            .tools
            .values()
            .map(|entry| entry.tool.clone())
            .collect())
    }

    async fn call(
        &self,
        call: ExtensionToolCall,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<ExtensionToolReply, DomainError> {
        if *cancel.borrow() {
            return Err(DomainError::cancelled(
                "Extension tool call cancelled before dispatch",
            ));
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        let sink = {
            let mut state = self.state.lock().expect("extension tool bridge poisoned");
            let Some(registration) = state
                .tools
                .get(&call.tool_id)
                .filter(|entry| entry.tool.enabled)
            else {
                return Ok(ExtensionToolReply::Error {
                    message: "This tool is currently unavailable. No action was taken.".into(),
                });
            };
            let sink = registration.sink.clone();
            let owner = registration.owner.clone();
            state
                .pending
                .insert(request_id.clone(), PendingCall { owner, sender });
            sink
        };

        // Delivery can block on the host transport and does not confirm page execution.
        if let Err(error) = sink.send(ExtensionToolEvent::Call {
            request_id: request_id.clone(),
            call,
        }) {
            self.state
                .lock()
                .expect("extension tool bridge poisoned")
                .pending
                .remove(&request_id);
            return Err(DomainError::InternalError(format!(
                "extension.tool_dispatch_failed: {error}"
            )));
        }

        let (result, abort_frontend) = tokio::select! {
            receipt = receiver => (receipt.map_err(|_| DomainError::InternalError(
                "extension.tool_disconnected: the page closed before confirming the tool result; the operation may have executed".into(),
            )), false),
            _ = cancel.wait_for(|cancelled| *cancelled) => (Err(DomainError::cancelled(
                "Extension tool call cancelled; already performed operations are not rolled back",
            )), true),
            _ = tokio::time::sleep(CALL_TIMEOUT) => (Ok(ExtensionToolReply::Error {
                message: "extension.tool_timeout: no tool result was received within 60 seconds. Cancellation was requested, but the operation may have partially completed or may still be running. Inspect the affected state before retrying, or choose another approach.".into(),
            }), true),
        };
        self.state
            .lock()
            .expect("extension tool bridge poisoned")
            .pending
            .remove(&request_id);
        if abort_frontend && let Err(error) = sink.send(ExtensionToolEvent::Cancel { request_id }) {
            tracing::warn!("Failed to deliver extension tool cancellation: {error}");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tt_contracts::extension_tools::ExtensionToolTarget;
    use tt_domain::models::tool::AgentToolScope;

    use super::*;

    struct TestSink(mpsc::UnboundedSender<Value>);

    impl ExtensionToolEventSink for TestSink {
        fn send(&self, event: ExtensionToolEvent) -> Result<(), String> {
            let value = serde_json::to_value(event).map_err(|error| error.to_string())?;
            self.0.send(value).map_err(|error| error.to_string())
        }
    }

    fn registered_bridge() -> (
        Arc<AgentExtensionTools>,
        ToolId,
        mpsc::UnboundedReceiver<Value>,
    ) {
        let bridge = Arc::new(AgentExtensionTools::default());
        let (sender, receiver) = mpsc::unbounded_channel();
        let sink: Arc<dyn ExtensionToolEventSink> = Arc::new(TestSink(sender));
        let tool_id = bridge
            .register(
                "main",
                ExtensionToolDefinition {
                    extension_id: "example".into(),
                    name: "inspect".into(),
                    description: "Inspect application state".into(),
                    input_schema: json!({"type": "object"}),
                    contexts: vec![AgentToolScope::Session],
                    enabled: true,
                },
                sink,
            )
            .unwrap();
        (bridge, tool_id, receiver)
    }

    fn tool_call(tool_id: ToolId) -> ExtensionToolCall {
        ExtensionToolCall {
            tool_id,
            run_id: "run".into(),
            invocation_id: "invocation".into(),
            call_id: "call".into(),
            target: ExtensionToolTarget::Session {
                session_id: "session".into(),
            },
            arguments: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn receipts_are_page_owned_and_reload_disconnects_pending_calls() {
        let (bridge, tool_id, mut events) = registered_bridge();
        let (_cancel, receiver) = watch::channel(false);
        let task = tokio::spawn({
            let bridge = bridge.clone();
            let call = tool_call(tool_id.clone());
            async move { bridge.call(call, receiver).await }
        });
        let event = events.recv().await.unwrap();
        let request_id = event["requestId"].as_str().unwrap();
        let reply = ExtensionToolReply::Value { value: Value::Null };
        assert!(!bridge.resolve("other", request_id, reply.clone()));
        assert!(bridge.resolve("main", request_id, reply.clone()));
        task.await
            .unwrap()
            .expect("accepted receipt should complete the call");
        assert!(!bridge.resolve("main", request_id, reply));

        let (_cancel, receiver) = watch::channel(false);
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.call(tool_call(tool_id), receiver).await }
        });
        let event = events.recv().await.unwrap();
        bridge.clear_page("main");
        assert!(
            matches!(task.await.unwrap(), Err(DomainError::InternalError(message)) if message.contains("extension.tool_disconnected"))
        );
        assert!(bridge.list().unwrap().is_empty());
        assert!(!bridge.resolve(
            "main",
            event["requestId"].as_str().unwrap(),
            ExtensionToolReply::Value { value: Value::Null }
        ));
    }
}
