//! The actix application: shared state, routes and middleware order.

use crate::access_log::access_log;
use crate::auth::require_api_key;
use crate::config::Config;
use crate::error::ApiError;
use crate::proxy::chat::chat_completions;
use crate::proxy::models::list_models;
use crate::proxy::upstream::build_http_client;
use crate::rate_limit::{enforce_rate_limit, RateLimiter};
use crate::stats::{get_stats, track_request, StatsStore};
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::middleware::from_fn;
use actix_web::{web, App, HttpRequest, HttpResponse, ResponseError};
use std::sync::Arc;

/// Everything the handlers share, built once at startup.
pub struct AppState {
    pub config: Config,
    /// One client for every upstream request, so connections are reused.
    pub http: reqwest::Client,
    pub stats: Arc<StatsStore>,
    pub limiter: RateLimiter,
}

impl AppState {
    pub fn new(config: Config, stats: Arc<StatsStore>) -> reqwest::Result<AppState> {
        let http = build_http_client(&config)?;
        let limiter = RateLimiter::new(config.rate_limit_rpm);
        Ok(AppState {
            config,
            http,
            stats,
            limiter,
        })
    }
}

/// Builds the app. `.wrap` calls run in reverse order, so inside `/v1` a
/// request passes Auth → RateLimit → Tracking → handler, and the access log
/// around everything also records requests that Auth or RateLimit reject.
pub fn build_app(
    state: web::Data<AppState>,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state.clone())
        // Body extractors read at most this much; larger bodies get a 413.
        .app_data(web::PayloadConfig::new(state.config.max_request_bytes))
        .wrap(from_fn(access_log))
        .route("/health", web::get().to(health))
        .service(
            web::scope("/v1")
                .wrap(from_fn(track_request))
                .wrap(from_fn(enforce_rate_limit))
                .wrap(from_fn(require_api_key))
                .route("/chat/completions", web::post().to(chat_completions))
                .route("/models", web::get().to(list_models))
                .route("/stats", web::get().to(get_stats)),
        )
        .default_service(web::to(not_found))
}

/// Public liveness probe for load balancers and orchestrators.
async fn health() -> HttpResponse {
    HttpResponse::Ok().body("ok")
}

async fn not_found(req: HttpRequest) -> HttpResponse {
    ApiError::not_found(format!("No route for {} {}.", req.method(), req.path())).error_response()
}
