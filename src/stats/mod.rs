//! Usage stats per key ID: the store, its file and `/v1/stats`.

mod http;
mod store;

pub use http::{get_stats, track_request, StatsQuery};
pub use store::{spawn_flusher, KeyStats, LoadOutcome, StatsLoadError, StatsStore};
