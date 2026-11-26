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
fn inspector_skips_an_empty_model() {
    let mut inspector = StreamInspector::new(false);
    inspector.inspect(b"data: {\"model\":\"\",\"choices\":[],\"usage\":null}\n\n");
    inspector.inspect(b"data: {\"model\":\"gpt-4o-2024-11-20\",\"choices\":[{\"index\":0}]}\n\n");
    assert_eq!(
        inspector.summary().model.as_deref(),
        Some("gpt-4o-2024-11-20")
    );
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
