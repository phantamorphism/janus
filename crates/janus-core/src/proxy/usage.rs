//! The `usage` observation: what a successful response says it consumed.
//!
//! All three launch upstreams report consumption as a JSON `usage` object,
//! either in a JSON response body or in one or more server-sent events of a
//! streamed one; they differ only in where in the JSON it sits. So one
//! observer serves them all, configured per upstream with those locations
//! ([`UsageShape`]). The observation is
//! `{"model": <model, if reported>, "usage": <the upstream's own usage
//! object>}` — the upstream's field names, untranslated, so a metering
//! plugin reads them against that upstream's documentation. When a stream
//! reports usage in several events (Anthropic's `message_start` then
//! `message_delta`), later fields overwrite earlier ones, which is how those
//! upstreams define the running totals.
//!
//! The observer only reads copies of the relayed chunks. A streamed body is
//! parsed event by event (only an unfinished event is held); a JSON body is
//! held up to [`JSON_BODY_LIMIT`] to be parsed at the end, and a larger one
//! goes unobserved rather than held; so does a single event larger than
//! [`SSE_EVENT_LIMIT`]. Either is logged as a warning when it leaves the
//! response with no observation, so a `usage: null` from size is visible to
//! the operator rather than silent.

use http::{header, HeaderMap, StatusCode};
use serde_json::{Map, Value};

use crate::contract::upstream::UsageObserver;

/// The largest JSON response body kept for observation.
pub(crate) const JSON_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// The largest single server-sent event (or line of one) kept for
/// observation. The same as a JSON body's: OpenAI's `response.completed`
/// event carries the whole response object, exactly what a non-streamed
/// call returns as its body.
const SSE_EVENT_LIMIT: usize = JSON_BODY_LIMIT;

/// Where one upstream reports consumption, as JSON paths into a response
/// body or an event's data.
pub(crate) struct UsageShape {
    /// Objects merged into the observation's `usage`, in this order.
    pub usage: &'static [&'static [&'static str]],
    /// Where the model name sits; the first found wins per document.
    pub model: &'static [&'static [&'static str]],
}

enum Mode {
    /// Not a successful JSON or event-stream response.
    Ignore,
    Json(Vec<u8>),
    Sse(SseParser),
    /// The JSON body outgrew [`JSON_BODY_LIMIT`].
    Overflowed,
}

pub(crate) struct ShapeObserver {
    shape: &'static UsageShape,
    mode: Mode,
    usage: Map<String, Value>,
    model: Option<String>,
}

impl ShapeObserver {
    pub fn new(shape: &'static UsageShape) -> Self {
        ShapeObserver {
            shape,
            mode: Mode::Ignore,
            usage: Map::new(),
            model: None,
        }
    }

    fn absorb(&mut self, doc: &Value) {
        for path in self.shape.usage {
            if let Some(Value::Object(fields)) = lookup(doc, path) {
                for (k, v) in fields {
                    if !v.is_null() {
                        self.usage.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if let Some(model) = self
            .shape
            .model
            .iter()
            .find_map(|path| lookup(doc, path).and_then(Value::as_str))
        {
            self.model = Some(model.to_owned());
        }
    }
}

fn lookup<'v>(doc: &'v Value, path: &[&str]) -> Option<&'v Value> {
    path.iter().try_fold(doc, |v, key| v.get(key))
}

impl UsageObserver for ShapeObserver {
    fn observe_head(&mut self, status: StatusCode, headers: &HeaderMap) {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let encoded = headers
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v.as_bytes() != b"identity");
        self.mode = if !status.is_success() || encoded {
            Mode::Ignore
        } else if content_type.starts_with("text/event-stream") {
            Mode::Sse(SseParser::default())
        } else if content_type.starts_with("application/json") {
            Mode::Json(Vec::new())
        } else {
            Mode::Ignore
        };
    }

    fn observe_chunk(&mut self, chunk: &[u8]) {
        match &mut self.mode {
            Mode::Ignore | Mode::Overflowed => {}
            Mode::Json(buf) => {
                if buf.len() + chunk.len() > JSON_BODY_LIMIT {
                    // Released now, not at the end of the body.
                    self.mode = Mode::Overflowed;
                } else {
                    buf.extend_from_slice(chunk);
                }
            }
            Mode::Sse(parser) => {
                let mut docs = Vec::new();
                parser.feed(chunk, |data| {
                    // Content deltas are most of a stream and never carry
                    // usage; skip parsing them once the model is known.
                    if mentions_usage(data) || self.model.is_none() {
                        if let Ok(doc) = serde_json::from_slice::<Value>(data) {
                            docs.push(doc);
                        }
                    }
                });
                for doc in &docs {
                    self.absorb(doc);
                }
            }
        }
    }

    fn finish(mut self: Box<Self>) -> Value {
        let oversized = match std::mem::replace(&mut self.mode, Mode::Ignore) {
            Mode::Json(buf) => {
                if let Ok(doc) = serde_json::from_slice::<Value>(&buf) {
                    self.absorb(&doc);
                }
                false
            }
            Mode::Overflowed => true,
            Mode::Sse(parser) => parser.dropped,
            Mode::Ignore => false,
        };
        if self.usage.is_empty() {
            if oversized {
                tracing::warn!(
                    limit = JSON_BODY_LIMIT,
                    "response too large to observe: its usage is not reported"
                );
            }
            return Value::Null;
        }
        let mut out = Map::new();
        if let Some(model) = self.model.take() {
            out.insert("model".into(), Value::String(model));
        }
        out.insert(
            "usage".into(),
            Value::Object(std::mem::take(&mut self.usage)),
        );
        Value::Object(out)
    }
}

fn mentions_usage(data: &[u8]) -> bool {
    data.windows(7).any(|w| w == b"\"usage\"")
}

/// Incremental server-sent-events framing: `data:` lines accumulated into
/// one event until a blank line, across chunk boundaries. A line ends at
/// LF, CR or CRLF, as the event-stream format allows.
#[derive(Default)]
struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    /// The current line or event outgrew [`SSE_EVENT_LIMIT`]; drop it.
    skipping: bool,
    /// The previous byte was a CR, so an LF now is the rest of a CRLF.
    after_cr: bool,
    /// Some event was dropped for its size.
    dropped: bool,
}

impl SseParser {
    fn feed(&mut self, chunk: &[u8], mut on_event: impl FnMut(&[u8])) {
        for &byte in chunk {
            let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => self.end_line(&mut on_event),
                _ if self.line.len() < SSE_EVENT_LIMIT => self.line.push(byte),
                _ => self.skipping = true,
            }
        }
    }

    fn end_line(&mut self, on_event: &mut impl FnMut(&[u8])) {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.has_data && !self.skipping {
                let data = std::mem::take(&mut self.data);
                if data != b"[DONE]" {
                    on_event(&data);
                }
            }
            self.dropped |= self.skipping;
            self.data.clear();
            self.has_data = false;
            self.skipping = false;
        } else if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if self.has_data {
                self.data.push(b'\n');
            }
            if self.data.len() + value.len() > SSE_EVENT_LIMIT {
                self.skipping = true;
            } else {
                self.data.extend_from_slice(value);
            }
            self.has_data = true;
        }
        // The line's allocation is reused for the next one.
        self.line = line;
        self.line.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SHAPE: UsageShape = UsageShape {
        usage: &[&["usage"], &["message", "usage"]],
        model: &[&["model"], &["message", "model"]],
    };

    fn observe(status: u16, content_type: &str, chunks: &[&[u8]]) -> Value {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        let mut observer: Box<dyn UsageObserver> = Box::new(ShapeObserver::new(&SHAPE));
        observer.observe_head(StatusCode::from_u16(status).unwrap(), &headers);
        for chunk in chunks {
            observer.observe_chunk(chunk);
        }
        observer.finish()
    }

    #[test]
    fn sse_events_split_across_chunks_at_any_byte_are_parsed() {
        let stream: &[u8] = b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\r\n\r\n: comment\n\ndata: {\"type\":\"message_delta\",\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\ndata: [DONE]\n\n";
        let whole = observe(200, "text/event-stream", &[stream]);
        assert_eq!(
            whole,
            serde_json::json!({"model": "m", "usage": {"input_tokens": 7, "output_tokens": 42}})
        );
        for split in 1..stream.len() {
            let (a, b) = stream.split_at(split);
            assert_eq!(
                observe(200, "text/event-stream", &[a, b]),
                whole,
                "split at {split}"
            );
        }
        let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(observe(200, "text/event-stream", &bytewise), whole);
    }

    #[test]
    fn cr_only_line_endings_are_lines() {
        let stream: &[u8] =
            b"event: x\rdata: {\"model\":\"m\",\rdata: \"usage\":{\"a\":1}}\r\rdata: {\"usage\":{\"b\":2}}\r\r";
        let whole = observe(200, "text/event-stream", &[stream]);
        assert_eq!(
            whole,
            serde_json::json!({"model": "m", "usage": {"a": 1, "b": 2}})
        );
        let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(observe(200, "text/event-stream", &bytewise), whole);
    }

    #[test]
    fn sse_memory_stays_within_the_event_limit() {
        // One endless line, fed in chunks: never held beyond the limit.
        let mut parser = SseParser::default();
        let mut events = 0;
        parser.feed(b"data: ", |_| events += 1);
        let block = vec![b'x'; 256 * 1024];
        for _ in 0..(3 * SSE_EVENT_LIMIT / block.len()) {
            parser.feed(&block, |_| events += 1);
            assert!(
                parser.line.len() <= SSE_EVENT_LIMIT,
                "{}",
                parser.line.len()
            );
        }
        parser.feed(b"\n\n", |_| events += 1);
        assert_eq!(events, 0, "the oversized event is dropped, not delivered");
        assert!(parser.dropped);

        // Many lines of one event, each under the limit: the event's data
        // is never held beyond it either.
        let mut parser = SseParser::default();
        let line = [
            b"data: ".as_slice(),
            &vec![b'y'; SSE_EVENT_LIMIT / 4],
            b"\n",
        ]
        .concat();
        for _ in 0..12 {
            parser.feed(&line, |_| events += 1);
            assert!(
                parser.data.len() <= SSE_EVENT_LIMIT,
                "{}",
                parser.data.len()
            );
        }
        parser.feed(b"\ndata: {\"usage\":{}}\n\n", |_| events += 1);
        assert_eq!(events, 1, "only the event after the oversized one");
    }

    #[test]
    fn multi_line_sse_data_is_one_event() {
        let got = observe(
            200,
            "text/event-stream; charset=utf-8",
            &[b"data: {\"usage\":\ndata: {\"total\":5}}\n\n"],
        );
        assert_eq!(got["usage"]["total"], 5);
    }

    #[test]
    fn json_body_observed_and_errors_or_other_types_ignored() {
        let body: &[u8] = br#"{"id":"x","model":"gpt","usage":{"prompt_tokens":3,"completion_tokens":4,"details":null}}"#;
        let (a, b) = body.split_at(20);
        assert_eq!(
            observe(200, "application/json", &[a, b]),
            serde_json::json!({"model": "gpt", "usage": {"prompt_tokens": 3, "completion_tokens": 4}})
        );
        assert_eq!(observe(400, "application/json", &[body]), Value::Null);
        assert_eq!(observe(200, "text/plain", &[body]), Value::Null);
        assert_eq!(
            observe(200, "application/json", &[b"{\"id\":1}"]),
            Value::Null
        );
        assert_eq!(
            observe(200, "application/json", &[b"not json"]),
            Value::Null
        );
    }

    #[test]
    fn oversized_bodies_go_unobserved_rather_than_held() {
        let filler = vec![b' '; JSON_BODY_LIMIT];
        assert_eq!(
            observe(
                200,
                "application/json",
                &[b"{\"usage\":{\"a\":1}", &filler, b"}"]
            ),
            Value::Null
        );
        let huge_event = [
            b"data: ".as_slice(),
            &vec![b'x'; SSE_EVENT_LIMIT + 1],
            b"\n\n",
        ]
        .concat();
        let after = b"data: {\"usage\":{\"a\":2}}\n\n";
        assert_eq!(
            observe(200, "text/event-stream", &[&huge_event, after])["usage"]["a"],
            2
        );
    }
}
