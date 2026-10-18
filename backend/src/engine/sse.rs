//! Server-sent events, matching `eventsource-parser` (dispatch on a blank
//! line when at least one `data:` line was seen; `event:` empty → undefined)
//! and `eventsource-encoder` (`event: x\n` + one `data: ` line per input line).

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Default)]
pub struct SseParser {
    pending: String,
    skip_lf: bool,
    data: Option<String>,
    event: Option<String>,
    id: Option<String>,
    first: bool,
}

impl SseParser {
    pub fn new() -> Self { Self { first: true, ..Default::default() } }

    /// Feed decoded text; returns completed events.
    pub fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        let mut chunk = chunk;
        if self.first {
            self.first = false;
            chunk = chunk.strip_prefix('\u{feff}').unwrap_or(chunk);
        }
        let mut out = Vec::new();
        self.pending.push_str(chunk);
        let buf = std::mem::take(&mut self.pending);
        let bytes = buf.as_bytes();
        let mut start = 0;
        let mut i = 0;
        if self.skip_lf && bytes.first() == Some(&b'\n') { start = 1; i = 1; }
        self.skip_lf = false;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => { self.line(&buf[start..i], &mut out); i += 1; start = i; }
                b'\r' => {
                    self.line(&buf[start..i], &mut out);
                    if i + 1 < bytes.len() { if bytes[i + 1] == b'\n' { i += 1; } } else { self.skip_lf = true; }
                    i += 1; start = i;
                }
                _ => i += 1,
            }
        }
        self.pending = buf[start..].to_owned();
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if let Some(data) = self.data.take() {
                out.push(SseEvent { event: self.event.take(), data, id: self.id.clone() });
            }
            self.event = None;
            return;
        }
        if line.starts_with(':') { return; }
        let (field, value) = match line.find(':') {
            Some(idx) => { let v = &line[idx + 1..]; (&line[..idx], v.strip_prefix(' ').unwrap_or(v)) }
            None => (line, ""),
        };
        match field {
            "data" => { self.data = Some(match self.data.take() { Some(d) => format!("{d}\n{value}"), None => value.to_owned() }); }
            "event" => { self.event = if value.is_empty() { None } else { Some(value.to_owned()) }; }
            "id" => { if !value.contains('\0') { self.id = Some(value.to_owned()); } }
            _ => {}
        }
    }
}

/// `encode({ event?, data })`.
pub fn encode(event: Option<&str>, data: &str) -> String {
    let mut out = String::new();
    if let Some(e) = event.filter(|e| !e.is_empty()) { out.push_str("event: "); out.push_str(e); out.push('\n'); }
    let normalized = data.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        out.push_str("data: ");
        out.push_str(line);
        out.push_str(if i + 1 == lines.len() { "\n\n" } else { "\n" });
    }
    out
}

/// `encode({ data })`.
pub fn data(data: &str) -> String { encode(None, data) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_split_and_crlf_streams() {
        let mut p = SseParser::new();
        let mut events = p.feed("event: a\r\nda");
        events.extend(p.feed("ta: x\r"));
        events.extend(p.feed("\n\r\ndata: 1\ndata: 2\n\n: c\n\n"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], SseEvent { event: Some("a".into()), data: "x".into(), id: None });
        assert_eq!(events[1].data, "1\n2");
        assert_eq!(encode(Some("e"), "a\nb"), "event: e\ndata: a\ndata: b\n\n");
    }
}
