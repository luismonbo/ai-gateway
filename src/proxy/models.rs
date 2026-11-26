//! `GET /v1/models`: every upstream's models in one OpenAI-style list, with
//! IDs prefixed so they can be sent straight back as `model`.

use super::upstream::{describe_failure, send_models};
use crate::app::AppState;
use crate::config::Upstream;
use actix_web::{web, HttpResponse};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

#[derive(Serialize, Debug, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Serialize)]
struct ModelList {
    object: &'static str,
    data: Vec<ModelEntry>,
}

/// The part of an upstream's list the gateway reads.
#[derive(Deserialize)]
struct UpstreamList {
    data: Vec<UpstreamModel>,
}

#[derive(Deserialize)]
struct UpstreamModel {
    id: String,
    /// Kept loose: anything but a whole number becomes 0.
    #[serde(default)]
    created: Value,
}

/// Reads an upstream's `{"data": [{"id": …}, …]}` and names each model
/// `{upstream}/{id}`, owned by the upstream.
pub fn prefix_models(upstream: &str, body: &[u8]) -> Result<Vec<ModelEntry>, serde_json::Error> {
    let list: UpstreamList = serde_json::from_slice(body)?;
    let models = list
        .data
        .into_iter()
        .map(|model| ModelEntry {
            id: format!("{upstream}/{}", model.id),
            object: "model",
            created: model.created.as_u64().unwrap_or(0),
            owned_by: upstream.to_string(),
        })
        .collect();
    Ok(models)
}

/// The models from `<NAME>_MODELS`, named `{upstream}/{model}` and owned by
/// the upstream. Nothing says when they were created, so `created` is 0.
pub fn listed_models(upstream: &str, models: &[String]) -> Vec<ModelEntry> {
    models
        .iter()
        .map(|model| ModelEntry {
            id: format!("{upstream}/{model}"),
            object: "model",
            created: 0,
            owned_by: upstream.to_string(),
        })
        .collect()
}

/// Asks every upstream at once, except those whose list is configured. An
/// upstream that fails is logged and left out, so the answer is always 200.
pub async fn list_models(state: web::Data<AppState>) -> HttpResponse {
    let lists = state
        .config
        .upstreams
        .iter()
        .map(|upstream| upstream_models(&state, upstream));
    // join_all keeps the input order, so the list follows UPSTREAMS.
    let data = join_all(lists).await.into_iter().flatten().collect();
    HttpResponse::Ok().json(ModelList {
        object: "list",
        data,
    })
}

async fn upstream_models(state: &AppState, upstream: &Upstream) -> Vec<ModelEntry> {
    // A configured list needs no request, so no token either.
    if let Some(models) = &upstream.models {
        return listed_models(&upstream.name, models);
    }
    let bearer = match state.sign_in.bearer(upstream).await {
        Ok(bearer) => bearer,
        Err(e) => {
            warn!(
                upstream = %upstream.name,
                error = %e,
                "could not sign in; leaving its models out"
            );
            return Vec::new();
        }
    };
    let timeout = state.config.connect_timeout;
    let body = match send_models(&state.http, upstream, bearer.as_ref(), timeout).await {
        Ok(body) => body,
        Err(failure) => {
            warn!(
                upstream = %upstream.name,
                error = %describe_failure(upstream, &failure),
                "could not list the upstream's models; leaving them out"
            );
            return Vec::new();
        }
    };
    prefix_models(&upstream.name, &body).unwrap_or_else(|e| {
        warn!(
            upstream = %upstream.name,
            error = %e,
            "the upstream's model list isn't an OpenAI-style list; leaving it out"
        );
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_models_prefixes_ids_and_sets_owner() {
        let body = br#"{"object":"list","data":[{"id":"gemma4:e2b","object":"model","created":1730000000,"owned_by":"library"}]}"#;
        assert_eq!(
            prefix_models("ollama", body).unwrap(),
            [ModelEntry {
                id: "ollama/gemma4:e2b".to_string(),
                object: "model",
                created: 1_730_000_000,
                owned_by: "ollama".to_string(),
            }]
        );
    }

    #[test]
    fn prefix_models_defaults_created_to_zero() {
        let body = br#"{"data":[{"id":"a"},{"id":"b","created":"yesterday"},{"id":"c","created":1.5},{"id":"d","created":-1}]}"#;
        let models = prefix_models("vllm", body).unwrap();
        assert_eq!(models.len(), 4);
        assert!(models.iter().all(|model| model.created == 0), "{models:?}");
    }

    #[test]
    fn listed_models_are_prefixed_and_owned() {
        let entry = |id: &str| ModelEntry {
            id: id.to_string(),
            object: "model",
            created: 0,
            owned_by: "azure".to_string(),
        };
        assert_eq!(
            listed_models("azure", &["a".to_string(), "b".to_string()]),
            [entry("azure/a"), entry("azure/b")]
        );
    }

    #[test]
    fn prefix_models_rejects_a_non_list() {
        for body in [
            &br#"{"data":"nope"}"#[..],
            br#"{"object":"list"}"#,
            br#"[{"id":"a"}]"#,
            b"<html>",
        ] {
            assert!(
                prefix_models("x", body).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }
}
