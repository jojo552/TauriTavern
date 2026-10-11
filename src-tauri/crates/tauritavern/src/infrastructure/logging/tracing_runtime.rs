use std::path::Path;
use std::sync::Arc;

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    prelude::*,
};

use tt_adapter_observability::devtools::BackendLogStore;
pub use tt_adapter_observability::user_errors::BackendErrorSink;
use tt_adapter_observability::user_errors::BackendUserErrorLayer;

pub struct TracingRuntimeGuard {
    _file_guard: tracing_appender::non_blocking::WorkerGuard,
}

pub fn init_tracing(
    log_dir: &Path,
    backend_log_store: Option<Arc<BackendLogStore>>,
    backend_error_sink: BackendErrorSink,
) -> Result<TracingRuntimeGuard, String> {
    std::fs::create_dir_all(log_dir)
        .map_err(|error| format!("Failed to create log directory {:?}: {}", log_dir, error))?;

    let file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "tauritavern.log");
    let (non_blocking, file_guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::Layer::new()
                .with_writer(std::io::stdout)
                .with_ansi(true)
                .with_span_events(FmtSpan::CLOSE)
                .with_target(true),
        )
        .with(
            fmt::Layer::new()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_span_events(FmtSpan::CLOSE)
                .with_target(true),
        )
        .with(backend_log_store.map(|store| store.layer()))
        .with(BackendUserErrorLayer::new(backend_error_sink));

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| format!("Failed to set global tracing subscriber: {error}"))?;

    tracing::debug!("Tracing initialized");
    Ok(TracingRuntimeGuard {
        _file_guard: file_guard,
    })
}
