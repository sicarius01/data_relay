use anyhow::Result;
use std::sync::OnceLock;
use tracing_subscriber::{
    layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter, Registry,
};
type ReloadHandle = reload::Handle<EnvFilter, Registry>;
static RELOAD: OnceLock<ReloadHandle> = OnceLock::new();
pub struct LoggingGuard {
    pub guard: tracing_appender::non_blocking::WorkerGuard,
}
impl LoggingGuard {
    pub fn reload_level(&self, level: &str) -> Result<()> {
        reload_level(level)
    }
}
pub fn init(config: &crate::config::LogConfig) -> Result<LoggingGuard> {
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("csv-agent")
        .filename_suffix("log")
        .max_log_files(config.max_files.max(1))
        .build(&config.dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let (filter, handle) = reload::Layer::new(EnvFilter::try_new(&config.level)?);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .try_init()?;
    let _ = RELOAD.set(handle);
    Ok(LoggingGuard { guard })
}
pub fn reload_level(level: &str) -> Result<()> {
    let filter = EnvFilter::try_new(level)?;
    if let Some(handle) = RELOAD.get() {
        handle.reload(filter)?;
    }
    Ok(())
}
