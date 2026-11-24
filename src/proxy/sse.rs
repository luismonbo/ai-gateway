//! Relaying an upstream's server-sent events to the client.
//!
//! Network chunks don't line up with events, so bytes are buffered until an
//! event is complete (it ends with a blank line). Each event is then
//! inspected for token usage and forwarded byte-for-byte.

use super::usage::{probe, Usage};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use serde_json::{Map, Value};
use std::fmt;
use tracing::warn;

/// A buffer this big without a complete event is passed on uninspected, so
/// an upstream that never sends a blank line can't grow it without limit.
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const STREAM_OPTIONS: &str = "stream_options";
const INCLUDE_USAGE: &str = "include_usage";

/// Cuts a byte stream into complete events.
#[derive(Default)]
pub struct SseFramer {
    buffer: BytesMut,
}

impl SseFramer {
    /// Adds `chunk` and returns every event now complete, each with its
    /// terminating blank line, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        self.buffer.extend_from_slice(chunk);
        let events: Vec<Bytes> = std::iter::from_fn(|| {
            event_len(&self.buffer).map(|len| self.buffer.split_to(len).freeze())
        })
        .collect();
        if self.buffer.len() > MAX_EVENT_BYTES {
            let oversized = self.buffer.split().freeze();
            return events.into_iter().chain([oversized]).collect();
        }
        events
    }

    /// Bytes after the last complete event, forwarded as they are.
    pub fn finish(self) -> Option<Bytes> {
        (!self.buffer.is_empty()).then(|| self.buffer.freeze())
    }
}

/// The length of the first complete event in `buffer`, or `None` if there
/// isn't one yet.
///
/// Lines end with "\n", "\r\n" or "\r", and an event ends at a blank line:
/// one line ending straight after another. As bytes that's "\n\n", "\n\r"
/// or "\r\r" ("\r\n" on its own is a single ending).
fn event_len(buffer: &[u8]) -> Option<usize> {
    let first = buffer
        .windows(2)
        .position(|pair| matches!(pair, b"\n\n" | b"\n\r" | b"\r\r"))?;
    let blank = first + 1; // where the blank line's own ending starts
    match (buffer[blank], buffer.get(blank + 1)) {
        (b'\n', _) => Some(blank + 1),
        (_, Some(b'\n')) => Some(blank + 2),
        (_, Some(_)) => Some(blank + 1),
        // A final "\r" may be half of "\r\n": wait for the next byte.
        (_, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAction {
    Forward,
    Drop,
}

/// What a finished stream reported, for stats.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSummary {
    pub usage: Option<Usage>,
    pub model: Option<String>,
}

/// Looks at each event: records usage and model, and decides whether the
/// client should see it.
pub struct StreamInspector {
    /// Set when the gateway asked for usage itself, so the client never sees
    /// the usage-only chunk it didn't ask for.
    strip_usage_chunk: bool,
    usage: Option<Usage>,
    model: Option<String>,
}

impl StreamInspector {
    pub fn new(strip_usage_chunk: bool) -> Self {
        Self {
            strip_usage_chunk,
            usage: None,
            model: None,
        }
    }

    /// Records the first `model` and the last `usage` seen. Drops only the
    /// usage-only chunk (`usage` plus `choices: []`), and only when asked to.
    pub fn inspect(&mut self, event: &[u8]) -> EventAction {
        let Some(data) = event_data(event) else {
            return EventAction::Forward;
        };
        if data == "[DONE]" {
            return EventAction::Forward;
        }
        let Some(chunk) = probe(data.as_bytes()) else {
            return EventAction::Forward;
        };
        if self.model.is_none() {
            self.model = chunk.model;
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }
        let usage_only = chunk.usage.is_some() && matches!(chunk.choices.as_deref(), Some([]));
        if self.strip_usage_chunk && usage_only {
            EventAction::Drop
        } else {
            EventAction::Forward
        }
    }

    pub fn summary(&self) -> StreamSummary {
        StreamSummary {
            usage: self.usage,
            model: self.model.clone(),
        }
    }
}

/// The event's `data:` lines joined with "\n", each without one leading
/// space. `None` if it has no data lines.
fn event_data(event: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event).ok()?;
    let lines: Vec<&str> = text.split(['\r', '\n']).filter_map(data_value).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The value of a `data` field line: `data: x` → `x`, `data` → empty.
fn data_value(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data")?;
    if rest.is_empty() {
        return Some("");
    }
    let value = rest.strip_prefix(':')?;
    Some(value.strip_prefix(' ').unwrap_or(value))
}

/// Makes sure the upstream reports usage at the end of the stream. Returns
/// the request's fields and whether the gateway added the option itself.
pub fn with_include_usage(rest: Map<String, Value>) -> (Map<String, Value>, bool) {
    let options = rest.get(STREAM_OPTIONS);
    let client_asked = options.and_then(|o| o.get(INCLUDE_USAGE)) == Some(&Value::Bool(true));
    if client_asked {
        return (rest, false);
    }
    // Keep the client's other stream options; anything that isn't an object
    // (such as null) is replaced.
    let kept = match options {
        Some(Value::Object(options)) => options.clone(),
        _ => Map::new(),
    };
    let options: Map<String, Value> = kept
        .into_iter()
        .chain([(INCLUDE_USAGE.to_string(), Value::Bool(true))])
        .collect();
    let rest = rest
        .into_iter()
        .chain([(STREAM_OPTIONS.to_string(), Value::Object(options))])
        .collect();
    (rest, true)
}

/// Passes `upstream`'s events to the client as they complete, and calls
/// `on_finish` with the summary once the stream ends.
///
/// If the upstream fails mid-stream the status has already been sent, so
/// the stream ends with an error, which aborts the client's connection
/// instead of ending the body as if the answer were complete.
pub fn relay_sse<S, E, F>(
    upstream: S,
    inspector: StreamInspector,
    on_finish: F,
) -> impl Stream<Item = Result<Bytes, actix_web::Error>> + 'static
where
    S: Stream<Item = Result<Bytes, E>> + 'static,
    E: fmt::Display + 'static,
    F: FnOnce(StreamSummary) + 'static,
{
    async_stream::stream! {
        let mut upstream = std::pin::pin!(upstream);
        let mut framer = SseFramer::default();
        let mut inspector = inspector;
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    warn!(error = %e, "upstream stream failed; aborting the response");
                    on_finish(inspector.summary());
                    yield Err(actix_web::error::ErrorBadGateway("upstream stream failed"));
                    return;
                }
            };
            for event in framer.push(&chunk) {
                if inspector.inspect(&event) == EventAction::Forward {
                    yield Ok(event);
                }
            }
        }
        // An unterminated last event: clients discard it, but its usage
        // still counts.
        if let Some(rest) = framer.finish() {
            if inspector.inspect(&rest) == EventAction::Forward {
                yield Ok(rest);
            }
        }
        on_finish(inspector.summary());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    const USAGE_ONLY: &[u8] =
        b"data: {\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n";

    fn events(framer: &mut SseFramer, chunk: &[u8]) -> Vec<Vec<u8>> {
        framer
            .push(chunk)
            .into_iter()
            .map(|event| event.to_vec())
            .collect()
    }

    #[test]
    fn framer_joins_an_event_split_across_chunks() {
        let mut framer = SseFramer::default();
        assert!(events(&mut framer, b"data: {\"a\"").is_empty());
        assert!(events(&mut framer, b":1}\n").is_empty());
        assert_eq!(
            events(&mut framer, b"\n"),
            [b"data: {\"a\":1}\n\n".to_vec()]
        );
        assert_eq!(framer.finish(), None);
    }

    #[test]
    fn framer_returns_several_events_from_one_chunk() {
        let mut framer = SseFramer::default();
        assert_eq!(
            events(&mut framer, b"data: 1\n\ndata: 2\n\ndata: 3"),
            [b"data: 1\n\n".to_vec(), b"data: 2\n\n".to_vec()]
        );
        assert_eq!(framer.finish().as_deref(), Some(&b"data: 3"[..]));
    }

    #[test]
    fn framer_handles_crlf_boundaries() {
        let mut framer = SseFramer::default();
        assert_eq!(
            events(&mut framer, b"data: x\r\n\r\n"),
            [b"data: x\r\n\r\n".to_vec()]
        );
        // A final "\r" may be half of "\r\n", so the framer waits for the next byte.
        assert!(events(&mut framer, b"data: y\r\n\r").is_empty());
        assert_eq!(events(&mut framer, b"\n"), [b"data: y\r\n\r\n".to_vec()]);
    }

    #[test]
    fn framer_accepts_every_line_ending() {
        let mut framer = SseFramer::default();
        assert_eq!(
            events(&mut framer, b"data: a\r\rdata: b\r\n\ndata: c\n\r\ndata: d"),
            [
                b"data: a\r\r".to_vec(),
                b"data: b\r\n\n".to_vec(),
                b"data: c\n\r\n".to_vec()
            ]
        );
    }

    #[test]
    fn framer_keeps_a_multibyte_char_split_across_chunks() {
        let bytes = "data: é\n\n".as_bytes();
        let split = bytes.iter().position(|&b| b == 0xC3).unwrap() + 1;
        let mut framer = SseFramer::default();
        assert!(events(&mut framer, &bytes[..split]).is_empty());
        assert_eq!(events(&mut framer, &bytes[split..]), [bytes.to_vec()]);
    }

    #[test]
    fn finish_returns_leftover_bytes() {
        let mut framer = SseFramer::default();
        assert!(events(&mut framer, b"data: partial").is_empty());
        assert_eq!(framer.finish().as_deref(), Some(&b"data: partial"[..]));
    }

    #[test]
    fn framer_passes_on_an_oversized_event_in_pieces() {
        let mut framer = SseFramer::default();
        let huge = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert_eq!(events(&mut framer, &huge), std::slice::from_ref(&huge));
        assert_eq!(events(&mut framer, b"\n\n"), [b"\n\n".to_vec()]);
    }

    #[test]
    fn inspector_records_usage_and_model() {
        let mut inspector = StreamInspector::new(false);
        let chunks = [
            &b"data: {\"model\":\"first\",\"choices\":[{}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n"[..],
            b"data: {\"model\":\"second\",\"choices\":[{}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\n",
        ];
        for chunk in chunks {
            assert_eq!(inspector.inspect(chunk), EventAction::Forward);
        }
        let summary = inspector.summary();
        assert_eq!(summary.model.as_deref(), Some("first"));
        assert_eq!(
            summary.usage,
            Some(Usage {
                prompt_tokens: 7,
                completion_tokens: 9
            })
        );
    }

    #[test]
    fn inspector_drops_the_injected_usage_chunk() {
        let mut inspector = StreamInspector::new(true);
        assert_eq!(inspector.inspect(USAGE_ONLY), EventAction::Drop);
        assert_eq!(
            inspector.summary().usage,
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 2
            })
        );
    }

    #[test]
    fn inspector_keeps_usage_when_client_asked() {
        let mut inspector = StreamInspector::new(false);
        assert_eq!(inspector.inspect(USAGE_ONLY), EventAction::Forward);
        assert!(inspector.summary().usage.is_some());
    }

    #[test]
    fn inspector_keeps_content_chunks_that_carry_usage() {
        let mut inspector = StreamInspector::new(true);
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n";
        assert_eq!(inspector.inspect(chunk), EventAction::Forward);
    }

    #[test]
    fn inspector_forwards_done_comments_and_non_json() {
        let mut inspector = StreamInspector::new(true);
        for event in [
            &b"data: [DONE]\n\n"[..],
            b": keep-alive\n\n",
            b"data: {nope\n\n",
            b"event: ping\n\n",
        ] {
            assert_eq!(
                inspector.inspect(event),
                EventAction::Forward,
                "{}",
                String::from_utf8_lossy(event)
            );
        }
        assert_eq!(inspector.summary(), StreamSummary::default());
    }

    #[test]
    fn inspector_joins_multiline_data() {
        let mut inspector = StreamInspector::new(true);
        let event = b"data:{\"choices\":[],\ndata: \"usage\":{\"prompt_tokens\":3}}\r\n\r\n";
        assert_eq!(inspector.inspect(event), EventAction::Drop);
        assert_eq!(inspector.summary().usage.unwrap().prompt_tokens, 3);
    }

    #[test]
    fn include_usage_is_injected_only_when_not_requested() {
        let object = |value: serde_json::Value| value.as_object().unwrap().clone();
        let with_options =
            |options: serde_json::Value| object(json!({"model": "m", "stream_options": options}));

        let (rest, injected) = with_include_usage(object(json!({"model": "m"})));
        assert!(injected);
        assert_eq!(rest["stream_options"], json!({"include_usage": true}));
        assert_eq!(rest["model"], "m");

        let asked = with_options(json!({"include_usage": true}));
        assert_eq!(with_include_usage(asked.clone()), (asked, false));

        let (rest, injected) = with_include_usage(with_options(json!({"include_usage": false})));
        assert!(injected);
        assert_eq!(rest["stream_options"], json!({"include_usage": true}));

        let (rest, injected) = with_include_usage(with_options(json!({"foo": 1})));
        assert!(injected);
        assert_eq!(
            rest["stream_options"],
            json!({"foo": 1, "include_usage": true})
        );

        let (rest, injected) = with_include_usage(with_options(json!(null)));
        assert!(injected);
        assert_eq!(rest["stream_options"], json!({"include_usage": true}));
    }

    #[actix_web::test]
    async fn relay_forwards_events_and_reports_summary() {
        let chunks = [
            &b"data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"h"[..],
            "é\"}}]}\n\n".as_bytes(),
            USAGE_ONLY,
            b"data: [DONE]\n\n",
        ];
        let upstream =
            futures::stream::iter(chunks.map(|chunk| Ok::<_, &str>(Bytes::copy_from_slice(chunk))));
        let seen = Rc::new(RefCell::new(None));
        let record = Rc::clone(&seen);

        let out: Vec<_> = relay_sse(upstream, StreamInspector::new(true), move |summary| {
            *record.borrow_mut() = Some(summary)
        })
        .collect()
        .await;

        let bytes: Vec<u8> = out
            .into_iter()
            .flat_map(|item| item.unwrap().to_vec())
            .collect();
        let expected = [chunks[0], chunks[1], chunks[3]].concat();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            String::from_utf8(expected).unwrap()
        );

        let summary = seen.borrow_mut().take().expect("on_finish ran");
        assert_eq!(summary.model.as_deref(), Some("m"));
        assert_eq!(summary.usage.unwrap().completion_tokens, 2);
    }

    #[actix_web::test]
    async fn relay_aborts_when_upstream_fails_mid_stream() {
        let upstream =
            futures::stream::iter(vec![Ok(Bytes::from_static(b"data: {}\n\n")), Err("boom")]);
        let finished = Rc::new(RefCell::new(false));
        let record = Rc::clone(&finished);
        let out: Vec<_> = relay_sse(upstream, StreamInspector::new(false), move |_| {
            *record.borrow_mut() = true
        })
        .collect()
        .await;
        assert_eq!(out.len(), 2);
        assert!(out[0].is_ok() && out[1].is_err());
        assert!(*finished.borrow(), "on_finish ran");
    }
}
