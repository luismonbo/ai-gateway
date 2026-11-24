//! The actix application: shared state, routes and middleware order.

use crate::auth::require_api_key;
use crate::config::Config;
use crate::error::ApiError;
use crate::handlers::get_stats;
use crate::middleware::{RateLimitMiddleware, RateLimiter, TrackingMiddleware};
use crate::proxy::chat::chat_completions;
use crate::proxy::upstream::build_http_client;
use crate::tracking::RequestTracker;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::middleware::{from_fn, Logger};
use actix_web::{web, App, HttpRequest, HttpResponse, ResponseError};
use std::sync::{Arc, RwLock};

/// Everything the handlers share, built once at startup.
pub struct AppState {
    pub config: Config,
    /// One client for every upstream request, so connections are reused.
    pub http: reqwest::Client,
    pub stats: Arc<RwLock<RequestTracker>>,
    pub limiter: Arc<RateLimiter>,
}

impl AppState {
    pub fn new(config: Config, stats: Arc<RwLock<RequestTracker>>) -> reqwest::Result<AppState> {
        let http = build_http_client(&config)?;
        Ok(AppState {
            config,
            http,
            stats,
            limiter: Arc::new(RateLimiter::new(60)),
        })
    }
}

/// Builds the app. `.wrap` calls run in reverse order, so inside `/v1` a
/// request passes Auth → RateLimit → Tracking → handler, and the `Logger`
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
        // The legacy stats handler reads the tracker directly until Task 14.
        .app_data(web::Data::from(state.stats.clone()))
        // Body extractors read at most this much; larger bodies get a 413.
        .app_data(web::PayloadConfig::new(state.config.max_request_bytes))
        .wrap(Logger::default())
        .route("/health", web::get().to(health))
        .service(
            web::scope("/v1")
                .wrap(TrackingMiddleware::new(state.stats.clone()))
                .wrap(RateLimitMiddleware::new(state.limiter.clone()))
                .wrap(from_fn(require_api_key))
                .route("/chat/completions", web::post().to(chat_completions))
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
