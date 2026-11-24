//! Picks the upstream for a request from its `model` field.
//!
//! `ollama/<model>` goes to the `ollama` upstream as `<model>`. A model
//! without a known prefix goes, unchanged, to `DEFAULT_UPSTREAM` if one is set.

/// Where a request goes, and the model name that upstream should see.
/// Both borrow from the inputs, so routing never copies strings.
#[derive(Debug, PartialEq)]
pub struct Route<'a> {
    pub upstream: &'a str,
    pub model: &'a str,
}

/// Every variant becomes a 400 `invalid_request_error` for the client.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum RouteError {
    #[error("`model` must not be empty.")]
    EmptyModel,
    #[error("Model '{upstream}/' names the upstream '{upstream}' but no model after the '/'.")]
    MissingModelAfterPrefix { upstream: String },
    #[error(
        "Model '{model}' doesn't start with a known upstream and no DEFAULT_UPSTREAM is set. \
         Prefix it with one of: {}.",
        valid_prefixes.join(", ")
    )]
    UnknownPrefix {
        model: String,
        valid_prefixes: Vec<String>,
    },
}

/// Splits `model` at its first `/`. If the text before it is exactly the name
/// of an upstream (case-sensitive), the request goes there with the rest as
/// the model. Otherwise the whole string goes to `default_upstream`.
pub fn resolve_route<'a>(
    model: &'a str,
    upstreams: &[&'a str],
    default_upstream: Option<&'a str>,
) -> Result<Route<'a>, RouteError> {
    if model.is_empty() {
        return Err(RouteError::EmptyModel);
    }
    if let Some((prefix, rest)) = model.split_once('/') {
        if upstreams.contains(&prefix) {
            if rest.is_empty() {
                return Err(RouteError::MissingModelAfterPrefix {
                    upstream: prefix.to_string(),
                });
            }
            return Ok(Route {
                upstream: prefix,
                model: rest,
            });
        }
    }
    match default_upstream {
        Some(upstream) => Ok(Route { upstream, model }),
        None => Err(RouteError::UnknownPrefix {
            model: model.to_string(),
            valid_prefixes: upstreams.iter().map(|name| format!("{name}/")).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSTREAMS: [&str; 2] = ["ollama", "openai"];

    #[test]
    fn routes_follow_the_spec_table() {
        let hf = "hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL";
        let prefixed_hf = format!("ollama/{hf}");
        let cases = [
            (
                "ollama/gemma4:e2b",
                Some("openai"),
                ("ollama", "gemma4:e2b"),
            ),
            ("openai/gpt-6-terra", None, ("openai", "gpt-6-terra")),
            (prefixed_hf.as_str(), None, ("ollama", hf)),
            (hf, Some("ollama"), ("ollama", hf)),
            ("gemma4:e2b", Some("ollama"), ("ollama", "gemma4:e2b")),
        ];
        for (model, default, (upstream, routed)) in cases {
            assert_eq!(
                resolve_route(model, &UPSTREAMS, default),
                Ok(Route {
                    upstream,
                    model: routed
                }),
                "{model}"
            );
        }
    }

    #[test]
    fn unprefixed_model_without_default_is_an_error() {
        let err = resolve_route("gemma4:e2b", &UPSTREAMS, None).unwrap_err();
        assert_eq!(
            err,
            RouteError::UnknownPrefix {
                model: "gemma4:e2b".to_string(),
                valid_prefixes: vec!["ollama/".to_string(), "openai/".to_string()],
            }
        );
        let message = err.to_string();
        assert!(message.contains("'gemma4:e2b'"), "{message}");
        assert!(message.contains("ollama/, openai/"), "{message}");
    }

    #[test]
    fn prefix_match_is_case_sensitive() {
        assert!(matches!(
            resolve_route("Ollama/x", &UPSTREAMS, None),
            Err(RouteError::UnknownPrefix { .. })
        ));
    }

    #[test]
    fn prefix_without_model_is_an_error() {
        let err = resolve_route("ollama/", &UPSTREAMS, Some("openai")).unwrap_err();
        assert_eq!(
            err,
            RouteError::MissingModelAfterPrefix {
                upstream: "ollama".to_string()
            }
        );
        assert!(err.to_string().contains("'ollama/'"), "{err}");
    }

    #[test]
    fn empty_model_is_an_error() {
        assert_eq!(
            resolve_route("", &UPSTREAMS, Some("ollama")),
            Err(RouteError::EmptyModel)
        );
    }
}
