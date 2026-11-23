use actix_web::{web, HttpServer};
use ai_gateway::app::{build_app, AppState};
use ai_gateway::auth::Role;
use ai_gateway::config::{Config, LogFormat};
use ai_gateway::logging::init_logging;
use ai_gateway::middleware::RateLimiter;
use ai_gateway::providers::{
    ollama::OllamaProvider, openai::OpenAIProvider, FallbackProvider, LLMProvider,
};
use ai_gateway::tracking::RequestTracker;
use std::env;
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
    info!(
        user_keys = config.keys.count(Role::User),
        admin_keys = config.keys.count(Role::Admin),
        "API keys loaded"
    );

    let stats = Arc::new(RwLock::new(load_stats()));
    let state = web::Data::new(AppState {
        config,
        stats: stats.clone(),
        limiter: Arc::new(RateLimiter::new(60)),
        provider: legacy_provider(),
    });

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

/// Today's Ollama-first provider with an OpenAI fallback. Replaced by
/// configured upstreams later in this branch.
fn legacy_provider() -> Arc<dyn LLMProvider> {
    let ollama = Arc::new(OllamaProvider::new(
        env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".to_string()),
    ));
    match (env::var("OPENAI_API_KEY"), env::var("OPENAI_BASE_URL")) {
        (Ok(key), Ok(url)) => Arc::new(FallbackProvider::new(
            ollama,
            Arc::new(OpenAIProvider::new(url, key)),
            Some("gpt-4.1-nano".to_string()),
        )),
        _ => ollama,
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
