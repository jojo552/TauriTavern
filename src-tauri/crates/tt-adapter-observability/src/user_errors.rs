//! Selection shared by both hosts; delivery and readiness belong to the host.
use std::sync::Arc;

use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tt_contracts::observability::USER_VISIBLE_ERROR;

pub type BackendErrorSink = Arc<dyn Fn(String) + Send + Sync + 'static>;

pub struct BackendUserErrorLayer {
    sink: BackendErrorSink,
}

impl BackendUserErrorLayer {
    pub fn new(sink: BackendErrorSink) -> Self {
        Self { sink }
    }
}

impl<S: Subscriber> Layer<S> for BackendUserErrorLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::ERROR
            || event.metadata().target() != USER_VISIBLE_ERROR
        {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            let message = message.trim();
            if !message.is_empty() {
                (self.sink)(message.to_string());
            }
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use tracing_subscriber::prelude::*;

    use super::*;

    #[test]
    fn both_hosts_select_only_explicit_errors_and_only_the_message() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let captured = messages.clone();
        let subscriber = tracing_subscriber::registry().with(BackendUserErrorLayer::new(Arc::new(
            move |message| captured.lock().unwrap().push(message),
        )));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "tauritavern::other", "ordinary error");
            tracing::warn!(target: USER_VISIBLE_ERROR, "warning");
            tracing::error!(target: USER_VISIBLE_ERROR, error = "private diagnostic field", " visible {} ", "error");
            tracing::error!(target: USER_VISIBLE_ERROR, "  ");
        });
        assert_eq!(*messages.lock().unwrap(), vec!["visible error"]);
    }
}
