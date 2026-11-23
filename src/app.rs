//! The actix application: shared state, routes and middleware order.

use crate::auth::require_api_key;
use crate::config::Config;
use crate::error::ApiError;
use crate::handlers::{chat_completions, get_stats};
use crate::middleware::{RateLimitMiddleware, RateLimiter, TrackingMiddleware};
use crate::providers::LLMProvider;
use crate::tracking::RequestTracker;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::middleware::{from_fn, Logger};
use actix_web::{web, App, HttpRequest, HttpResponse, ResponseError};
use std::sync::{Arc, RwLock};

/// Everything the handlers share, built once at startup.
pub struct AppState {
    pub config: Config,
    pub stats: Arc<RwLock<RequestTracker>>,
    pub limiter: Arc<RateLimiter>,
    pub provider: Arc<dyn LLMProvider>,
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
        // The legacy handlers read these directly until their replacements land.
        .app_data(web::Data::from(state.stats.clone()))
        .app_data(web::Data::from(state.provider.clone()))
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
