use async_trait::async_trait;
use tokio::sync::watch;
use tt_contracts::extension_tools::{
    ExtensionTool, ExtensionToolCall, ExtensionToolEvent, ExtensionToolReply,
};
use tt_domain::errors::DomainError;

#[async_trait]
pub trait ExtensionTools: Send + Sync {
    fn list(&self) -> Result<Vec<ExtensionTool>, DomainError>;

    async fn call(
        &self,
        call: ExtensionToolCall,
        cancel: watch::Receiver<bool>,
    ) -> Result<ExtensionToolReply, DomainError>;
}

/// Delivers tool events to the page that registered the tool.
///
/// Each host supplies its own transport (a WebView IPC channel, a browser event stream).
/// Delivery only means the event was handed to that transport; the page confirms execution
/// separately by resolving the call.
pub trait ExtensionToolEventSink: Send + Sync {
    fn send(&self, event: ExtensionToolEvent) -> Result<(), String>;
}
