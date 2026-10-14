//! The thin layer between `tiny_http` and the pure router: a parsed request, a reply, and
//! the code that moves bytes. Everything above this works on [`Req`] and [`Reply`] only.

use std::io::Read;

use serde_json::Value;
use tiny_http::{Header, Response};

use crate::Ctx;

/// Largest request body accepted; every API body is a few hundred bytes.
pub(crate) const MAX_BODY: usize = 64 * 1024;

/// A request reduced to what the handlers use.
#[derive(Debug, Default)]
pub(crate) struct Req {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub host: Option<String>,
    pub origin: Option<String>,
    /// The `X-Token` header.
    pub token: Option<String>,
    pub body: Vec<u8>,
    pub too_large: bool,
}

impl Req {
    /// Value of query parameter `key`.
    pub fn q(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Request as it would arrive from a browser on this server; for tests.
    #[cfg(test)]
    pub fn test(method: &str, target: &str, port: u16, token: Option<&str>) -> Req {
        let (path, query) = split_target(target);
        Req {
            method: method.into(),
            path,
            query,
            host: Some(format!("127.0.0.1:{port}")),
            token: token.map(String::from),
            ..Req::default()
        }
    }
}

/// A response before it is written to the socket.
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// An HTML page: gets the restrictive CSP and frame headers.
    pub is_page: bool,
}

impl Reply {
    pub fn json(status: u16, value: &Value) -> Reply {
        Reply {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()),
            is_page: false,
        }
    }

    pub fn error(status: u16, message: &str) -> Reply {
        Reply::json(status, &serde_json::json!({ "error": message }))
    }

    pub fn asset(content_type: &'static str, body: &'static str) -> Reply {
        Reply { status: 200, content_type, body: body.as_bytes().to_vec(), is_page: false }
    }

    pub fn page(status: u16, html: &str) -> Reply {
        Reply {
            status,
            content_type: "text/html; charset=utf-8",
            body: html.as_bytes().to_vec(),
            is_page: true,
        }
    }

    pub fn empty(status: u16) -> Reply {
        Reply { status, content_type: "text/plain; charset=utf-8", body: Vec::new(), is_page: false }
    }
}

/// Read the request, route it (a panicking handler becomes a 500, not a dead worker) and
/// write the reply.
pub(crate) fn serve(ctx: &Ctx, mut request: tiny_http::Request) {
    let req = read_request(&mut request);
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::router::route(ctx, &req)
    }))
    .unwrap_or_else(|_| {
        log::error!("ui: handler panicked for {} {}", req.method, req.path);
        Reply::error(500, "internal error")
    });
    if let Err(e) = request.respond(into_response(reply)) {
        log::debug!("ui: client went away: {e}");
    }
}

fn read_request(request: &mut tiny_http::Request) -> Req {
    let method = request.method().as_str().to_ascii_uppercase();
    let (path, query) = split_target(request.url());
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    let (host, origin, token) = (header("Host"), header("Origin"), header("X-Token"));

    let mut body = Vec::new();
    let mut too_large = false;
    if method != "GET" && method != "HEAD" {
        if request.body_length().is_some_and(|n| n > MAX_BODY) {
            too_large = true;
        } else {
            let mut limited = request.as_reader().take(MAX_BODY as u64 + 1);
            if limited.read_to_end(&mut body).is_err() {
                body.clear();
            }
            too_large = body.len() > MAX_BODY;
        }
    }
    Req { method, path, query, host, origin, token, body, too_large }
}

fn into_response(reply: Reply) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut resp = Response::from_data(reply.body).with_status_code(reply.status);
    let mut add = |name: &str, value: &str| {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            resp.add_header(h);
        }
    };
    add("Content-Type", reply.content_type);
    add("Cache-Control", "no-store");
    add("X-Content-Type-Options", "nosniff");
    add("Referrer-Policy", "no-referrer");
    add("Cross-Origin-Resource-Policy", "same-origin");
    if reply.is_page {
        add("X-Frame-Options", "DENY");
        add(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline' \
             https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
             connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; \
             frame-ancestors 'none'",
        );
    }
    resp
}

/// Split `/path?a=1&b=2` into the path and decoded query pairs.
pub(crate) fn split_target(target: &str) -> (String, Vec<(String, String)>) {
    let target = target.split('#').next().unwrap_or("");
    match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.to_string(), Vec::new()),
    }
}

pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// `%XX` and `+` decoding; invalid escapes are kept as they are.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let pair = bytes.get(i + 1).copied().and_then(hex).zip(bytes.get(i + 2).copied().and_then(hex));
                match pair {
                    Some((hi, lo)) => {
                        out.push((hi << 4) | lo);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_split_and_decoded() {
        let (path, q) = split_target("/api/events?limit=50&kind=fix&x=a%20b+c&flag&bad=%zz&e=%C3%A9#frag");
        assert_eq!(path, "/api/events");
        let get = |k: &str| q.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("limit"), Some("50"));
        assert_eq!(get("kind"), Some("fix"));
        assert_eq!(get("x"), Some("a b c"));
        assert_eq!(get("flag"), Some(""));
        assert_eq!(get("bad"), Some("%zz"));
        assert_eq!(get("e"), Some("é"));
        assert_eq!(split_target("/").0, "/");
        assert!(split_target("/x").1.is_empty());
    }

    #[test]
    fn trailing_percent_is_kept() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%41"), "A");
    }
}
