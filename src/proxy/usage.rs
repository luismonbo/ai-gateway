//! Reading `model` and `usage` out of an upstream's JSON, for stats. The
//! JSON itself is forwarded untouched; this only looks.

use serde::de::IgnoredAny;
use serde::Deserialize;

/// Token counts, as OpenAI-compatible APIs report them.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

/// The few fields the gateway reads from a response or a stream chunk.
/// Everything else is skipped without being stored.
#[derive(Deserialize, Debug, Default)]
pub struct ResponseProbe {
    pub model: Option<String>,
    pub usage: Option<Usage>,
    /// Only the count matters: `choices: []` marks a usage-only chunk.
    pub choices: Option<Vec<IgnoredAny>>,
}

/// `None` unless `json` is a JSON object whose fields have the expected types.
pub fn probe(json: &[u8]) -> Option<ResponseProbe> {
    // serde would also accept an array and fill the fields in order.
    let is_object = json.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    is_object
        .then(|| serde_json::from_slice(json).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reads_model_usage_and_choices() {
        let body = br#" {"id":"x","model":"gemma4:e2b","choices":[{"index":0}],
            "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#;
        let probe = probe(body).unwrap();
        assert_eq!(probe.model.as_deref(), Some("gemma4:e2b"));
        assert_eq!(
            probe.usage,
            Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 3
            })
        );
        assert_eq!(probe.choices.map(|choices| choices.len()), Some(1));
    }

    #[test]
    fn probe_defaults_missing_fields() {
        let partial = probe(br#"{"usage":{"prompt_tokens":4}}"#).unwrap();
        assert_eq!(partial.model, None);
        assert_eq!(partial.usage.unwrap().completion_tokens, 0);
        assert!(partial.choices.is_none());

        let null_usage = probe(br#"{"usage":null}"#).unwrap();
        assert_eq!(null_usage.usage, None);
    }

    #[test]
    fn probe_rejects_anything_but_a_json_object() {
        for body in [&b"[\"gemma4\", null, []]"[..], b"\"text\"", b"{nope", b""] {
            assert!(probe(body).is_none(), "{}", String::from_utf8_lossy(body));
        }
    }
}
