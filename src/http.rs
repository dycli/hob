//! The little of HTTP the server needs, over tiny_http: a request read
//! whole (bodies here are small JSON), and a response that is bytes or a
//! stream relayed as it arrives (live video, clips).
//!
//! Every connection gets its own thread, as everything upstream is a
//! blocking call. A house has a handful of clients, and a live view holds
//! one thread for as long as it plays, which a thread does cheaply.

use std::io::Read;
use std::net::IpAddr;

/// A request as the routes see it.
pub struct Request {
    pub method: String,
    pub path: String,
    query: Vec<(String, String)>,
    /// A browser names its page; MCP clients send no Origin.
    pub origin: bool,
    pub peer: Option<IpAddr>,
    pub body: Vec<u8>,
}

/// The largest request body read; nothing the house takes is larger.
const MOST: u64 = 1 << 20;

impl Request {
    pub fn read(r: &mut tiny_http::Request) -> Self {
        let url = r.url().to_owned();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let header = |name: &str| {
            r.headers()
                .iter()
                .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                .map(|h| h.value.as_str().to_owned())
        };
        let origin = header("Origin").is_some();
        let mut body = Vec::new();
        if let Err(e) = r.as_reader().take(MOST).read_to_end(&mut body) {
            eprintln!("request body: {e}");
        }
        Request {
            method: r.method().as_str().to_owned(),
            path: decode(path),
            query: query
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    (decode(k), decode(v))
                })
                .collect(),
            origin,
            peer: r.remote_addr().map(|a| a.ip()),
            body,
        }
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn number(&self, key: &str) -> Option<f64> {
        self.query(key).and_then(|v| v.parse().ok())
    }
}

/// Percent-decoding, `+` as a space, as forms and query strings use.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub enum Body {
    Bytes(Vec<u8>),
    /// Relayed until it ends or the client goes; its length if known.
    Stream(Box<dyn Read + Send>, Option<usize>),
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
}

impl Response {
    pub fn bytes(status: u16, kind: &str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            headers: vec![("Content-Type", kind.to_owned())],
            body: Body::Bytes(body.into()),
        }
    }

    pub fn json(value: &impl serde::Serialize) -> Self {
        Self::bytes(
            200,
            "application/json",
            serde_json::to_vec(value).unwrap_or_default(),
        )
        .cache("no-cache")
    }

    pub fn text(status: u16, text: &str) -> Self {
        Self::bytes(status, "text/plain; charset=utf-8", text)
    }

    pub fn empty(status: u16) -> Self {
        Self::bytes(status, "text/plain", Vec::new())
    }

    pub fn stream(kind: &str, body: Box<dyn Read + Send>, length: Option<usize>) -> Self {
        Response {
            status: 200,
            headers: vec![("Content-Type", kind.to_owned())],
            body: Body::Stream(body, length),
        }
    }

    pub fn cache(mut self, policy: &str) -> Self {
        self.headers.push(("Cache-Control", policy.to_owned()));
        self
    }

    /// Writes the response; a client gone mid-stream ends the relay, which
    /// drops the upstream connection with it.
    pub fn send(self, r: tiny_http::Request) {
        let headers: Vec<tiny_http::Header> = self
            .headers
            .iter()
            .filter_map(|(k, v)| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).ok())
            .collect();
        let status = tiny_http::StatusCode(self.status);
        let sent = match self.body {
            Body::Bytes(b) => {
                let n = b.len();
                r.respond(tiny_http::Response::new(
                    status,
                    headers,
                    std::io::Cursor::new(b),
                    Some(n),
                    None,
                ))
            }
            Body::Stream(s, n) => r.respond(tiny_http::Response::new(status, headers, s, n, None)),
        };
        if let Err(e) = sent
            && !matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            )
        {
            eprintln!("respond: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_reads_escapes_and_plus() {
        assert_eq!(decode("a%20b+c"), "a b c");
        assert_eq!(decode("%E2%9C%93"), "✓");
        assert_eq!(decode("50%"), "50%");
        assert_eq!(decode("%zz"), "%zz");
    }
}
