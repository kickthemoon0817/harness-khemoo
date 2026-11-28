//! Serves the last published dashboard, its sections, the client script and readiness, on
//! 127.0.0.1 only.
//!
//! A request whose Host header is not `127.0.0.1:<port>` or `localhost:<port>` gets 421, so a
//! page that rebinds its own name to 127.0.0.1 cannot read the dashboard. Every route requires
//! this start's access token, compared in constant time; the request log masks it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value;
use subtle::ConstantTimeEq;

use super::collector::Collector;
use super::render;
use super::text::{sha256_hex, utc};

const SCRIPT: &str = include_str!("dashboard.js");
const SECURITY_HEADERS: [(&str, &str); 7] = [
    (
        "Content-Security-Policy",
        "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    ),
    ("Cross-Origin-Resource-Policy", "same-origin"),
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "private, no-cache"),
    ("Vary", "Accept-Encoding"),
];
const HTML: &str = "text/html; charset=utf-8";
const JSON: &str = "application/json; charset=utf-8";

/// Ready-to-send representations, each validated against its exact wire bytes.
pub struct Payload {
    pub content_type: &'static str,
    pub plain: Bytes,
    pub plain_tag: String,
    pub compressed: Bytes,
    pub compressed_tag: String,
}

fn tag_of(bytes: &[u8]) -> String {
    format!("\"{}\"", sha256_hex(bytes))
}

impl Payload {
    pub fn prepare(content_type: &'static str, body: &str) -> Arc<Self> {
        let plain = body.as_bytes().to_vec();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(3));
        encoder.write_all(&plain).ok();
        let compressed = encoder.finish().unwrap_or_default();
        Arc::new(Self {
            content_type,
            plain_tag: tag_of(&plain),
            compressed_tag: tag_of(&compressed),
            plain: Bytes::from(plain),
            compressed: Bytes::from(compressed),
        })
    }
}

type Routes = HashMap<String, Arc<Payload>>;

#[derive(Default)]
struct Published {
    routes: Arc<Routes>,
    prior_fragments: Arc<Routes>,
}

/// What the handlers read: the published route table, the Host names answered to and the
/// access token of this start.
pub struct Server {
    pub collector: Arc<Collector>,
    pub refresh_s: u64,
    pub access_token: String,
    pub port: u16,
    secret_values: Vec<String>,
    static_routes: Routes,
    allowed_hosts: [String; 2],
    published: RwLock<Published>,
}

impl Server {
    pub fn new(
        collector: Arc<Collector>,
        refresh_s: u64,
        secret_values: &[String],
        access_token: String,
        port: u16,
    ) -> Self {
        let secret_values: Vec<String> = secret_values
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();
        let mut server = Self {
            collector,
            refresh_s: refresh_s.max(1),
            access_token,
            port,
            secret_values,
            static_routes: HashMap::new(),
            allowed_hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
            published: RwLock::new(Published::default()),
        };
        let script = server.scrub(SCRIPT);
        server.static_routes.insert(
            "/dashboard.js".to_owned(),
            Payload::prepare("text/javascript; charset=utf-8", &script),
        );
        server.static_routes.insert(
            "/healthz".to_owned(),
            Payload::prepare("text/plain; charset=utf-8", "ok\n"),
        );
        server
    }

    /// The page's URL, access token included.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?t={}", self.port, self.access_token)
    }

    /// Every secret value removed from a response body.
    pub fn scrub(&self, body: &str) -> String {
        let mut body = body.to_owned();
        for value in &self.secret_values {
            body = body.replace(value.as_str(), "[redacted]");
        }
        body
    }

    fn scrub_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.scrub(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.scrub_value(item)),
            Value::Object(map) => {
                let entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
                for (key, mut item) in entries {
                    self.scrub_value(&mut item);
                    map.insert(self.scrub(&key), item);
                }
            }
            _ => {}
        }
    }

    /// Collects and prepares one generation before swapping the handlers' route table.
    pub fn publish_in_place(&self) {
        self.publish_snapshot(self.collector.snapshot());
    }

    pub fn publish_snapshot(&self, mut snapshot: Value) {
        let mut raw = snapshot.to_string();
        let leaks = self.secret_values.iter().any(|value| {
            let quoted = serde_json::to_string(value).unwrap_or_default();
            raw.contains(&quoted[1..quoted.len() - 1])
        });
        if leaks {
            self.scrub_value(&mut snapshot);
            raw = snapshot.to_string();
        }
        let previous = Arc::clone(
            &self
                .published
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .routes,
        );
        if previous
            .get("/api/state")
            .is_some_and(|state| state.plain == raw.as_bytes())
        {
            return;
        }
        let sections: Vec<(String, String)> = render::sections(&snapshot, &self.access_token)
            .into_iter()
            .map(|(name, fragment)| (name, self.scrub(&fragment)))
            .collect();
        let mut routes: Routes = self.static_routes.clone();
        let mut versions = Vec::new();
        for (name, fragment) in &sections {
            let digest = sha256_hex(fragment.as_bytes());
            let path = format!("/api/sections/{digest}");
            let payload = previous
                .get(&path)
                .cloned()
                .unwrap_or_else(|| Payload::prepare(HTML, fragment));
            routes.insert(path, payload);
            versions.push((name.clone(), digest));
        }
        let page = render::document(
            &sections,
            &versions,
            self.refresh_s,
            &self.access_token,
            snapshot["title"].as_str().unwrap_or_default(),
        );
        let page = Payload::prepare(HTML, &self.scrub(&page));
        routes.insert("/".to_owned(), Arc::clone(&page));
        routes.insert("/index.html".to_owned(), page);
        routes.insert("/api/state".to_owned(), Payload::prepare(JSON, &raw));
        // The index keeps the page's section order, which the client follows to order the machines.
        let index: Vec<String> = versions
            .iter()
            .map(|(name, digest)| {
                format!(
                    "{}:{}",
                    Value::String(name.clone()),
                    Value::String(digest.clone())
                )
            })
            .collect();
        routes.insert(
            "/api/sections".to_owned(),
            Payload::prepare(JSON, &format!("{{{}}}", index.join(","))),
        );
        // A tab can receive the old index just before publication. Keep its fragments for one interval.
        let prior: Routes = previous
            .iter()
            .filter(|(path, _)| path.starts_with("/api/sections/"))
            .map(|(path, body)| (path.clone(), Arc::clone(body)))
            .collect();
        let mut published = self
            .published
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        published.routes = Arc::new(routes);
        published.prior_fragments = Arc::new(prior);
    }

    pub fn route(&self, path: &str) -> Option<Arc<Payload>> {
        let published = self
            .published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        published
            .routes
            .get(path)
            .or_else(|| published.prior_fragments.get(path))
            .cloned()
    }

    fn handle(&self, request: &Request<Incoming>) -> Response<Full<Bytes>> {
        if request.method() != Method::GET {
            return error(
                StatusCode::NOT_IMPLEMENTED,
                &self.scrub("unsupported method\n"),
            );
        }
        let host = request
            .headers()
            .get(hyper::header::HOST)
            .and_then(|host| host.to_str().ok())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if !self.allowed_hosts.contains(&host) {
            return error(
                StatusCode::MISDIRECTED_REQUEST,
                &self.scrub("misdirected request: use 127.0.0.1 or localhost\n"),
            );
        }
        let given = query_value(request.uri().query().unwrap_or_default(), "t");
        if !bool::from(given.as_bytes().ct_eq(self.access_token.as_bytes())) {
            return error(
                StatusCode::FORBIDDEN,
                &self.scrub("forbidden: open the URL that bin/dashboard.sh status prints\n"),
            );
        }
        let Some(payload) = self.route(request.uri().path()) else {
            return error(StatusCode::NOT_FOUND, &self.scrub("not found\n"));
        };
        let weights = encoding_weights(request.headers());
        let star = weights.get("*").copied();
        let gzip_q = weights.get("gzip").copied().or(star).unwrap_or(0.0);
        let identity_q = weights
            .get("identity")
            .copied()
            .unwrap_or(if star == Some(0.0) { 0.0 } else { 1.0 });
        let use_gzip = gzip_q > 0.0 && gzip_q >= weights.get("identity").copied().unwrap_or(0.0);
        if !use_gzip && identity_q <= 0.0 {
            return error(
                StatusCode::NOT_ACCEPTABLE,
                &self.scrub("no acceptable encoding\n"),
            );
        }
        let (tag, body) = if use_gzip {
            (&payload.compressed_tag, &payload.compressed)
        } else {
            (&payload.plain_tag, &payload.plain)
        };
        let candidates = joined(request.headers(), hyper::header::IF_NONE_MATCH);
        let matched = candidates.split(',').any(|item| {
            let item = item.trim();
            let item = item.strip_prefix("W/").unwrap_or(item);
            item == tag || item == "*"
        });
        let mut response =
            Response::new(Full::new(if matched { Bytes::new() } else { body.clone() }));
        *response.status_mut() = if matched {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        };
        let headers = response.headers_mut();
        insert(headers, "ETag", tag);
        if use_gzip {
            insert(headers, "Content-Encoding", "gzip");
        }
        if !matched {
            insert(headers, "Content-Type", payload.content_type);
            insert(headers, "Content-Length", &body.len().to_string());
        }
        secure(headers);
        response
    }

    /// Serves on `listener` until `shutdown` resolves.
    pub async fn serve(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
    ) {
        tokio::pin!(shutdown);
        loop {
            let accepted = tokio::select! {
                () = &mut shutdown => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, peer)) = accepted else {
                // Out of descriptors, say: wait rather than spin until one frees.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let server = Arc::clone(&server);
                    async move {
                        let response = server.handle(&request);
                        server.log(peer, &request, response.status());
                        Ok::<_, Infallible>(response)
                    }
                });
                hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(30))
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .ok();
            });
        }
    }

    /// Logs the request line and status, with the access token masked and control characters
    /// escaped, and never the headers.
    fn log(&self, peer: SocketAddr, request: &Request<Incoming>, status: StatusCode) {
        let mut line = format!(
            "\"{} {} {:?}\" {} -",
            request.method(),
            request.uri(),
            request.version(),
            status.as_u16()
        );
        if !self.access_token.is_empty() {
            line = line.replace(self.access_token.as_str(), "[token]");
        }
        let line: String = line
            .chars()
            .map(|character| {
                if character.is_control() {
                    format!("\\x{:02x}", u32::from(character))
                } else {
                    character.to_string()
                }
            })
            .collect();
        let now = utc(super::collector::wall_clock());
        eprintln!(
            "{} - - [{:02}/{}/{:04} {:02}:{:02}:{:02}] {line}",
            peer.ip(),
            now.day(),
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
            ][usize::from(u8::from(now.month())) - 1],
            now.year(),
            now.hour(),
            now.minute(),
            now.second()
        );
    }
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn secure(headers: &mut HeaderMap) {
    headers.insert("Server", HeaderValue::from_static("harness-dashboard"));
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

/// A response with the security headers set.
fn error(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_owned())));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    insert(headers, "Content-Type", "text/plain; charset=utf-8");
    insert(headers, "Content-Length", &body.len().to_string());
    secure(headers);
    response
}

fn joined(headers: &HeaderMap, name: hyper::header::HeaderName) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// Each coding's weight from Accept-Encoding; a weight that does not read is 0.
fn encoding_weights(headers: &HeaderMap) -> HashMap<String, f64> {
    let mut weights = HashMap::new();
    for entry in joined(headers, hyper::header::ACCEPT_ENCODING)
        .to_lowercase()
        .split(',')
    {
        let (coding, parameter) = entry.split_once(';').unwrap_or((entry, ""));
        let quality = parameter.trim();
        let quality = quality.strip_prefix("q=").unwrap_or(quality);
        let weight = if parameter.is_empty() {
            1.0
        } else {
            quality.trim().parse().unwrap_or(0.0)
        };
        weights.insert(coding.trim().to_owned(), weight);
    }
    weights
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = |byte: u8| char::from(byte).to_digit(16);
                if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    decoded.extend(u8::try_from(high * 16 + low));
                    index += 3;
                    continue;
                }
                decoded.push(b'%');
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The first non-empty value of `name` in a query string, `+` and `%XX` decoded.
fn query_value(query: &str, name: &str) -> String {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(_, value)| !value.is_empty())
        .find(|(key, _)| percent_decode(key) == name)
        .map(|(_, value)| percent_decode(value))
        .unwrap_or_default()
}

/// A random access token: 24 bytes, URL-safe base64 without padding.
pub fn new_access_token() -> std::io::Result<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 24];
    getrandom::getrandom(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut token = String::with_capacity(32);
    for chunk in bytes.chunks(3) {
        let group = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        for shift in [18, 12, 6, 0] {
            token.push(char::from(ALPHABET[((group >> shift) & 63) as usize]));
        }
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_url_safe() {
        let (first, second) = (
            new_access_token().expect("random"),
            new_access_token().expect("random"),
        );
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
    }

    #[test]
    fn the_query_reads_as_parse_qs_reads_it() {
        assert_eq!(query_value("t=abc&x=1", "t"), "abc");
        assert_eq!(query_value("t=&t=b", "t"), "b");
        assert_eq!(query_value("x=abc", "t"), "");
        assert_eq!(query_value("t=a%2Db", "t"), "a-b");
    }
}
