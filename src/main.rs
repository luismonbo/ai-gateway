use actix_web::{web, HttpServer};
use ai_gateway::app::{build_app, AppState};
use ai_gateway::config::{Config, LogFormat};
use ai_gateway::logging::init_logging;
use ai_gateway::tracking::RequestTracker;
use std::process::ExitCode;
use std::sync::{Arc, PoisonError, RwLock};
use tracing::{error, info, warn};

const STATS_PATH: &str = "stats.json";

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

    let stats = Arc::new(RwLock::new(load_stats()));
    let state = match AppState::new(config, stats.clone()) {
        Ok(state) => web::Data::new(state),
        Err(e) => {
            error!("could not build the HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };

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

    let result = server.run().await;
    info!("server stopped; saving stats");
    save_stats(&stats);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("server error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load_stats() -> RequestTracker {
    match RequestTracker::load_from_file(STATS_PATH) {
        Ok(tracker) => {
            info!("loaded request stats from {STATS_PATH}");
            tracker
        }
        Err(_) => {
            info!("no existing stats found, starting fresh");
            RequestTracker::new()
        }
    }
}

fn save_stats(stats: &RwLock<RequestTracker>) {
    let tracker = stats.read().unwrap_or_else(PoisonError::into_inner);
    match tracker.save_to_file(STATS_PATH) {
        Ok(()) => info!("saved request stats to {STATS_PATH}"),
        Err(e) => error!("could not save request stats: {e}"),
    }
}
