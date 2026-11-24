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

/// A buffer this big without a complete event is passed on as it is (the
/// inspector can't parse a partial event, so it's forwarded), so an upstream
/// that never sends a blank line can't grow the buffer without limit.
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
mod tests;
