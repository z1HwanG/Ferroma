//! Test-only helpers: a hand-written HTTP/1.1 mock server and a temp directory.
//!
//! The mock server is deliberately *not* a dependency. It speaks just enough
//! HTTP/1.1 to exercise the FCP client — request line, headers, a
//! `Content-Length` body, and responses that are either complete, truncated, or
//! reset — which is exactly what the sync engine's failure paths need. It never
//! touches the network beyond `127.0.0.1` and it is compiled only for tests.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Re-exported so test modules share one temp-directory implementation.
pub(crate) use tempfile::TempDir;

/// One request the mock server received.
#[derive(Debug, Clone)]
pub(crate) struct RecordedRequest {
    /// `GET`, `POST`, `PATCH`, …
    pub method: String,
    /// The path, without the query string.
    pub path: String,
    /// The raw query string (empty when there was none).
    pub query: String,
    /// Header name (lowercased) to value.
    pub headers: HashMap<String, String>,
    /// The request body.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// The body parsed as JSON.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// A header value, case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Query parameters, split on `&` and `=`.
    pub(crate) fn query_params(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for pair in self.query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.insert(k.to_string(), v.to_string());
        }
        out
    }
}

/// What the mock server should answer with.
#[derive(Debug, Clone)]
pub(crate) enum MockResponse {
    /// A complete response.
    Full {
        /// HTTP status code.
        status: u16,
        /// Extra headers, appended after `Content-Type: application/json`.
        headers: Vec<(String, String)>,
        /// The body.
        body: Vec<u8>,
    },
    /// Close the connection without writing anything (a connection reset).
    Reset,
    /// Write `head` bytes and then reset — a truncated body, mid-transfer.
    Truncate {
        /// The bytes to flush before resetting.
        head: Vec<u8>,
    },
}

impl MockResponse {
    /// A `200` JSON response.
    pub(crate) fn json(body: impl Into<String>) -> Self {
        MockResponse::json_status(200, body)
    }

    /// A JSON response with an explicit status.
    pub(crate) fn json_status(status: u16, body: impl Into<String>) -> Self {
        MockResponse::Full {
            status,
            headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())],
            body: body.into().into_bytes(),
        }
    }

    /// A JSON response with extra headers (used for `Retry-After`).
    pub(crate) fn json_with_headers(
        status: u16,
        body: impl Into<String>,
        headers: Vec<(&str, &str)>,
    ) -> Self {
        MockResponse::Full {
            status,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.into().into_bytes(),
        }
    }

    /// The documented error envelope (`docs/api.md` §1.3).
    pub(crate) fn error(status: u16, code: &str, message: &str) -> Self {
        MockResponse::json_status(
            status,
            format!(
                r#"{{"error":{{"code":"{}","message":"{}"}}}}"#,
                code,
                message.replace('"', "'")
            ),
        )
    }

    /// A JSON body that is not valid JSON at all.
    pub(crate) fn malformed(status: u16, body: &str) -> Self {
        MockResponse::json_status(status, body)
    }
}

type Handler = Box<dyn Fn(&RecordedRequest, usize) -> MockResponse + Send + Sync>;

struct Route {
    method: String,
    path: String,
    handler: Handler,
    hits: AtomicUsize,
}

#[derive(Default)]
struct ServerState {
    routes: Mutex<Vec<Arc<Route>>>,
    requests: Mutex<Vec<RecordedRequest>>,
}

/// A tiny HTTP/1.1 server for tests.
pub(crate) struct MockServer {
    addr: SocketAddr,
    state: Arc<ServerState>,
}

impl MockServer {
    /// Bind to an ephemeral port on loopback and start serving.
    pub(crate) async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(ServerState::default());
        let task_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let conn_state = Arc::clone(&task_state);
                tokio::spawn(async move {
                    serve_connection(stream, conn_state).await;
                });
            }
        });
        MockServer { addr, state }
    }

    /// The root the FCP client should be pointed at.
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The FCP base URL including the protocol prefix.
    pub(crate) fn fcp_base_url(&self) -> String {
        format!("http://{}/api/v1/client", self.addr)
    }

    /// Register a handler for `METHOD path`. The handler receives the request and
    /// how many times this route has been hit (0 for the first).
    pub(crate) fn route<F>(&self, method: &str, path: &str, handler: F)
    where
        F: Fn(&RecordedRequest, usize) -> MockResponse + Send + Sync + 'static,
    {
        let mut routes = self.state.routes.lock().expect("routes lock");
        routes.push(Arc::new(Route {
            method: method.to_ascii_uppercase(),
            path: path.to_string(),
            handler: Box::new(handler),
            hits: AtomicUsize::new(0),
        }));
    }

    /// Register a fixed JSON answer.
    pub(crate) fn json_route(&self, method: &str, path: &str, status: u16, body: impl Into<String>) {
        let body = body.into();
        self.route(method, path, move |_req, _n| MockResponse::json_status(status, body.clone()));
    }

    /// Register a fixed error envelope (`docs/api.md` §1.3).
    pub(crate) fn error_route(
        &self,
        method: &str,
        path: &str,
        status: u16,
        code: &str,
        message: &str,
    ) {
        let code = code.to_string();
        let message = message.to_string();
        self.route(method, path, move |_req, _n| {
            MockResponse::error(status, &code, &message)
        });
    }

    /// The requests that used `method` on `path`, in arrival order.
    pub(crate) fn calls(&self, method: &str, path: &str) -> Vec<RecordedRequest> {
        self.requests_for(path)
            .into_iter()
            .filter(|r| r.method == method.to_ascii_uppercase())
            .collect()
    }

    /// Register a sequence of answers: the *n*-th hit gets the *n*-th response,
    /// and every further hit repeats the last one. This is how a
    /// `401 → refresh → 200` flow is scripted.
    pub(crate) fn script(&self, method: &str, path: &str, responses: Vec<MockResponse>) {
        self.route(method, path, move |_req, n| {
            let index = n.min(responses.len().saturating_sub(1));
            responses
                .get(index)
                .cloned()
                .unwrap_or_else(|| MockResponse::error(500, "internal_error", "no scripted response"))
        });
    }

    /// Every request the server has seen, in arrival order.
    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().expect("requests lock").clone()
    }

    /// The requests whose path equals `path`.
    pub(crate) fn requests_for(&self, path: &str) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.path == path)
            .collect()
    }

    /// How many requests the server has seen in total.
    pub(crate) fn request_count(&self) -> usize {
        self.state.requests.lock().expect("requests lock").len()
    }

    /// How many requests matched `path`.
    pub(crate) fn count_for(&self, path: &str) -> usize {
        self.requests_for(path).len()
    }
}

async fn serve_connection(mut stream: TcpStream, state: Arc<ServerState>) {
    loop {
        let request = match read_request(&mut stream).await {
            Some(request) => request,
            None => return,
        };

        let keep_alive = request
            .header("connection")
            .map(|v| !v.eq_ignore_ascii_case("close"))
            .unwrap_or(true);
        let response = {
            let routes = state.routes.lock().expect("routes lock");
            let mut selected = None;
            for route in routes.iter() {
                if route.method == request.method && request.path.starts_with(&route.path) {
                    selected = Some(Arc::clone(route));
                    break;
                }
            }
            selected.map(|route| {
                let n = route.hits.fetch_add(1, Ordering::SeqCst);
                (route.handler)(&request, n)
            })
        };
        state.requests.lock().expect("requests lock").push(request);

        let response = match response {
            Some(response) => response,
            None => MockResponse::error(404, "not_found", "no route"),
        };

        match response {
            MockResponse::Full {
                status,
                headers,
                body,
            } => {
                let mut out = Vec::new();
                out.extend_from_slice(format!("HTTP/1.1 {status} {}\r\n", reason(status)).as_bytes());
                out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
                for (name, value) in &headers {
                    out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
                }
                out.extend_from_slice(
                    format!(
                        "Connection: {}\r\n\r\n",
                        if keep_alive { "keep-alive" } else { "close" }
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(&body);
                if stream.write_all(&out).await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
                if !keep_alive {
                    return;
                }
            }
            MockResponse::Reset => {
                reset(&mut stream).await;
                return;
            }
            MockResponse::Truncate { head } => {
                let _ = stream.write_all(&head).await;
                let _ = stream.flush().await;
                reset(&mut stream).await;
                return;
            }
        }
    }
}

/// Force a TCP reset rather than an orderly close, so the peer sees a transport
/// error mid-response instead of a clean EOF.
#[allow(deprecated)] // SO_LINGER with a zero timeout is how a TCP RST is forced.
async fn reset(stream: &mut TcpStream) {
    let _ = stream.set_linger(Some(std::time::Duration::ZERO));
    // This is `AsyncWriteExt::shutdown`; dropping the future would do nothing.
    let _ = stream.shutdown().await;
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        if let Some(pos) = find_header_end(&buffer) {
            header_end = pos;
            break;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > 1024 * 1024 {
            return None;
        }
    }

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let body_start = header_end + 4;
    let mut body = buffer[body_start.min(buffer.len())..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Some(RecordedRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

/// A JSON `error` envelope with extra `details`.
pub(crate) fn error_envelope(code: &str, message: &str, details: serde_json::Value) -> String {
    serde_json::json!({ "error": { "code": code, "message": message, "details": details } })
        .to_string()
}
