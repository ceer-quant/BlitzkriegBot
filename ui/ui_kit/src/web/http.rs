use super::*;

/// Minimal parsed HTTP request.
pub(super) struct HttpRequest {
    pub(super) method: String,
    pub(super) target: String,
    pub(super) body: String,
    /// Raw header (name, value) pairs, lowercased names.
    pub(super) headers: Vec<(String, String)>,
}

impl HttpRequest {
    pub(super) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
    /// Case-insensitive header lookup over the stored lines.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        // Header case is handled by the caller storing the raw lines; keep the
        // contract tiny: headers were captured lowercased by read_request.
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    /// First `cmd` value from the query string, percent-decoded (`+` → space).
    pub(super) fn query_cmd(&self) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("cmd=") {
                return Some(url_decode(v));
            }
        }
        None
    }

    /// E25 (#331): one decoded `name=value` query parameter, for the
    /// intent-audit proxy's pass-through filters.
    pub(super) fn query_param(&self, name: &str) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k == name {
                return Some(url_decode(v));
            }
        }
        None
    }
}

pub(super) fn url_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse one HTTP/1.1 request (request line + headers + optional body).
pub(super) fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim();
        if h.is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some((name, value)) = h.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let body = if content_length > 0 {
        let mut buf = vec![0u8; content_length.min(64 * 1024)];
        let mut read = 0;
        while read < buf.len() {
            match reader.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
        buf.truncate(read);
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    };
    Some(HttpRequest {
        method,
        target,
        body,
        headers,
    })
}

/// POST bodies may be raw command text or `{"cmd":"..."}`.
/// Parse a login request body: JSON `{"user": “…”, “password”: “…”}` (or the
/// url-encoded `user=…&password=…` form). Returns None on malformed input.
pub(super) fn parse_login_body(body: &str) -> Option<(String, String)> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let user = v.get("user")?.as_str()?.to_string();
        let password = v.get("password")?.as_str()?.to_string();
        Some((user, password))
    } else {
        let mut user = None;
        let mut password = None;
        for pair in trimmed.split('&') {
            if let Some(v) = pair.strip_prefix("user=") {
                user = Some(url_decode(v));
            } else if let Some(v) = pair.strip_prefix("password=") {
                password = Some(url_decode(v));
            }
        }
        Some((user?, password?))
    }
}

pub(super) fn body_to_command(body: &str) -> String {
    let t = body.trim();
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(c) = v.get("cmd").and_then(|c| c.as_str()) {
                return c.to_string();
            }
        }
    }
    t.to_string()
}

/// #353: parse a POST body as the JSON params object a backtest IPC call
/// takes. Nothing is interpreted, filtered or reshaped — the object travels
/// verbatim to the kernel, whose typed deserialization is the validator.
pub(super) fn body_params(body: &str) -> Result<serde_json::Value, String> {
    let t = body.trim();
    if t.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(t).map_err(|e| format!("body must be a JSON object: {e}"))
}
