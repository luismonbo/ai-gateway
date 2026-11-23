//! Log output: human-readable text or JSON lines, filtered by `RUST_LOG`.

use crate::config::LogFormat;
use tracing::warn;
use tracing_subscriber::EnvFilter;

const DEFAULT_FILTER: &str = "info";

/// Installs the global log subscriber. Call it once, after `.env` is loaded,
/// so `RUST_LOG` set there applies.
pub fn init_logging(format: LogFormat) {
    let rust_log_is_set = std::env::var_os("RUST_LOG").is_some();
    let (filter, bad_filter) = match EnvFilter::try_from_default_env() {
        Ok(filter) => (filter, None),
        Err(e) => (
            EnvFilter::new(DEFAULT_FILTER),
            rust_log_is_set.then(|| e.to_string()),
        ),
    };

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let installed = match format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
    if let Err(e) = installed {
        eprintln!("logging was not set up: {e}");
    }

    if let Some(reason) = bad_filter {
        warn!("RUST_LOG is not a valid filter ({reason}); using \"{DEFAULT_FILTER}\"");
    }
}
