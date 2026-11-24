//! Log output: human-readable text or JSON lines, filtered by `RUST_LOG`.

use crate::config::LogFormat;
use std::ffi::OsStr;
use std::io::IsTerminal;
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

    let ansi = use_ansi(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi);
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

/// Colour codes only when a person is reading a terminal, and never when
/// `NO_COLOR` is set to something (see no-color.org). Anywhere else, such
/// as Docker logs or a file, they'd show up as `[32m` noise.
fn use_ansi(stdout_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    stdout_is_terminal && no_color.is_none_or(OsStr::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn colours_only_for_a_terminal_without_no_color() {
        assert!(use_ansi(true, None));
        // Docker, systemd, `> file`: no escape codes in the log.
        assert!(!use_ansi(false, None));
        assert!(!use_ansi(true, Some(OsStr::new("1"))));
        // no-color.org: only a non-empty NO_COLOR turns colours off.
        assert!(use_ansi(true, Some(OsStr::new(""))));
    }
}
