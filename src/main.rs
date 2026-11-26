use actix_web::{web, HttpServer};
use ai_gateway::app::{build_app, AppState};
use ai_gateway::config::{Config, LogFormat};
use ai_gateway::logging::init_logging;
use ai_gateway::stats::{spawn_flusher, LoadOutcome, StatsStore};
use azure_core::time::OffsetDateTime;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use tracing::{error, info, warn};

#[actix_web::main]
async fn main() -> ExitCode {
    // Load .env before anything reads the environment, so LOG_FORMAT and
    // RUST_LOG set there apply to logging. Its result is reported once
    // logging is up.
    let dotenv = dotenvy::dotenv();
    let config = Config::from_env();
    let log_format = config
        .as_ref()
        .map(|(config, _)| config.log_format)
        .unwrap_or(LogFormat::Text);
    init_logging(log_format);

    match dotenv {
        Ok(path) => info!("loaded environment from {}", path.display()),
        Err(e) if e.not_found() => info!("no .env file; using the process environment"),
        // The parse error holds the offending line, which may hold a secret.
        Err(dotenvy::Error::LineParse(..)) => {
            error!(".env has a syntax error; fix it and restart");
            return ExitCode::FAILURE;
        }
        Err(dotenvy::Error::Io(e)) => {
            error!("could not read .env: {e}");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            error!("could not load .env");
            return ExitCode::FAILURE;
        }
    }

    let (config, warnings) = match config {
        Ok(loaded) => loaded,
        Err(e) => {
            error!("invalid configuration: {e}");
            return ExitCode::FAILURE;
        }
    };
    for warning in &warnings {
        warn!("{warning}");
    }
    for line in config.summary_lines() {
        info!("{line}");
    }

    let stats_path = config.stats_path.clone();
    let flush_every = config.stats_flush_interval;
    let stats = match StatsStore::open(&stats_path) {
        Ok((stats, outcome)) => {
            log_load(&stats_path, &outcome);
            Arc::new(stats)
        }
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let state = match AppState::new(config, Arc::clone(&stats)) {
        Ok(state) => web::Data::new(state),
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    log_sign_in(&state).await;

    let address = (state.config.host.clone(), state.config.port);
    let server = HttpServer::new(move || build_app(state.clone())).bind(address.clone());
    let server = match server {
        Ok(server) => server,
        Err(e) => {
            error!("could not listen on {}:{}: {e}", address.0, address.1);
            return ExitCode::FAILURE;
        }
    };
    info!("listening on http://{}:{}", address.0, address.1);

    let flusher = spawn_flusher(Arc::clone(&stats), stats_path.clone(), flush_every);
    let result = server.run().await;
    flusher.abort();

    info!("server stopped; saving stats");
    let saved = stats.save(&stats_path);
    if let Err(e) = &saved {
        error!("could not save stats to {}: {e}", stats_path.display());
    }
    match result {
        Ok(()) if saved.is_ok() => ExitCode::SUCCESS,
        Ok(()) => ExitCode::FAILURE,
        Err(e) => {
            error!("server error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Signs in to each Azure upstream once, so a problem shows at startup. A
/// failure only warns: the other upstreams work, and this one's requests
/// fail until it can sign in.
async fn log_sign_in(state: &AppState) {
    for (name, result) in state.sign_in.check().await {
        let method = state
            .config
            .upstream(&name)
            .and_then(|upstream| upstream.sign_in.as_ref())
            .map(|sign_in| sign_in.method.to_string())
            .unwrap_or_default();
        match result {
            Ok(expires_on) => {
                let minutes = (expires_on - OffsetDateTime::now_utc()).whole_minutes();
                info!("upstream {name}: signed in with {method}; token valid for {minutes} min");
            }
            Err(e) => warn!(
                "upstream {name}: could not sign in with {method}: {e}; its requests fail \
                 until it can"
            ),
        }
    }
}

fn log_load(path: &Path, outcome: &LoadOutcome) {
    let path = path.display();
    match outcome {
        LoadOutcome::Missing => info!("no stats file at {path}; starting empty"),
        LoadOutcome::Loaded { keys, rekeyed } => {
            info!("loaded stats for {keys} keys from {path}");
            if *rekeyed > 0 {
                warn!(
                    "{rekeyed} stats entries were stored under raw API keys; they now use \
                     key IDs, and {path} no longer holds the raw keys"
                );
            }
        }
        LoadOutcome::Quarantined { moved_to, reason } => error!(
            "stats file {path} was unusable ({reason}); moved it to {} and starting empty",
            moved_to.display()
        ),
    }
}
