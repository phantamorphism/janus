//! An incremental `text/event-stream` decoder: enough of the SSE format for
//! the daemon's live tail (`event:` and `data:` fields, comments, CRLF or LF
//! line ends, events split across chunks).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field; `message` when absent, as SSE defines.
    pub event: String,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl Decoder {
    /// Feed one chunk; returns every event it completed.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if let Some(event) = self.dispatch() {
                    events.push(event);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line.as_ref(), ""),
            };
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                // `id` and `retry` are not used by the daemon's tail.
                _ => {}
            }
        }
        events
    }

    fn dispatch(&mut self) -> Option<Event> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        Some(Event {
            event: event
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| "message".to_owned()),
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_events_split_across_chunks_with_comments_and_crlf() {
        let mut d = Decoder::default();
        assert!(d.feed(b"event: ping\ndata: {\"at\"").is_empty());
        let events = d.feed(b":\"now\"}\n\n: keep-alive\n\nevent:request\r\ndata:{}\r\n\r\n");
        assert_eq!(
            events,
            vec![
                Event {
                    event: "ping".into(),
                    data: "{\"at\":\"now\"}".into()
                },
                Event {
                    event: "request".into(),
                    data: "{}".into()
                },
            ]
        );
        let multi = d.feed(b"data: a\ndata: b\n\n");
        assert_eq!(multi[0].event, "message");
        assert_eq!(multi[0].data, "a\nb");
        assert!(d.feed(b"event: empty\n\n").is_empty(), "no data, no event");
    }
}
