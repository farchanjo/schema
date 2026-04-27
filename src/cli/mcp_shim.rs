//! `schema mcp-shim` — stdio ↔ HTTP MCP bridge per ADR-0030.
//!
//! Claude Code (or any other MCP client) spawns this binary as a stdio MCP
//! server. Each line on stdin is one newline-delimited JSON-RPC frame; the
//! shim forwards it as a `POST /mcp` to the schema daemon, injecting the
//! bearer token from the global `endpoint.toml` written by the daemon on
//! startup (ADR-0021 + ADR-0027). Responses are written back to stdout as
//! one JSON object per line.
//!
//! On `401 Unauthorized` the shim re-reads `endpoint.toml` once and
//! retries — the daemon rotates the bearer on every restart, and that
//! rotation is the failure mode this shim exists to absorb. On a
//! connection-level failure (`ECONNREFUSED`, request-build timeout) the
//! shim does the same: re-read `endpoint.toml` once (the URL may have
//! changed too) and retry. A second failure of either class is fatal —
//! we do not loop indefinitely; the operator's MCP client should surface
//! the error.
//!
//! All logging goes to stderr via `tracing`. The shim **must never** write
//! to stdout outside of JSON-RPC frames, otherwise the client's framing
//! breaks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, stdin, stdout};

use crate::adapters::endpoint_toml::Endpoint;

/// HTTP header carrying the `rmcp` `StreamableHttp` session id. The daemon
/// emits this header on the response to `initialize` and requires it on
/// every subsequent POST to /mcp; without it the daemon answers
/// `422 Unprocessable Entity — Unexpected message, expect initialize
/// request`. The shim caches the value across calls so Claude Code's
/// `initialize → notifications/initialized → tools/call` sequence
/// stitches across the per-request POST/HTTP boundary.
const MCP_SESSION_HEADER: &str = "mcp-session-id";

/// Reqwest client timeout per forwarded request. `synthesize` calls can
/// be long-running (LLM round-trip); 2 minutes leaves headroom without
/// hanging Claude Code forever on a wedged daemon.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(2);

/// Run the shim until stdin closes (EOF) or an unrecoverable error surfaces.
///
/// # Errors
///
/// Returns `Err` when the global `endpoint.toml` cannot be read on first
/// call, when stdin / stdout I/O fails, when the daemon returns a non-401
/// non-success status, or when both retry attempts fail after a 401 or
/// connection failure. Errors are reported to stderr by `main` and the
/// process exits non-zero; Claude Code surfaces this as the MCP server
/// failing.
pub async fn run(endpoint_path: PathBuf) -> Result<()> {
    let mut endpoint = load_endpoint(&endpoint_path)?;
    let client = build_client()?;
    bridge_loop(&client, &mut endpoint, &endpoint_path).await
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building reqwest::Client for mcp-shim")
}

fn load_endpoint(path: &Path) -> Result<Endpoint> {
    Endpoint::load(path).with_context(|| {
        format!(
            "reading {} — is the schema daemon running? \
             (start it via `schema install --daemon` + launchctl load, or \
             `schema daemon` for a foreground run)",
            path.display()
        )
    })
}

async fn bridge_loop(
    client: &reqwest::Client,
    endpoint: &mut Endpoint,
    endpoint_path: &Path,
) -> Result<()> {
    let mut reader = BufReader::new(stdin());
    let mut writer = stdout();
    let mut line = String::new();
    let mut session = SessionState::default();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .context("reading JSON-RPC frame from stdin")?;
        if read == 0 {
            tracing::debug!("mcp-shim: stdin EOF, exiting");
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        let outcome =
            forward_with_retry(client, endpoint, endpoint_path, trimmed, &mut session).await?;
        if let Some(body) = outcome {
            write_frame(&mut writer, &body).await?;
        }
    }
}

/// Cross-call state the shim holds between POSTs. Currently only the
/// `rmcp` `StreamableHttp` session id; future fields (e.g., last-event-id
/// for resumable streams) extend this without changing the bridge
/// contract.
#[derive(Debug, Default)]
struct SessionState {
    session_id: Option<HeaderValue>,
}

async fn write_frame<W>(writer: &mut W, body: &str) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    writer
        .write_all(body.as_bytes())
        .await
        .context("writing JSON-RPC response to stdout")?;
    writer
        .write_all(b"\n")
        .await
        .context("writing JSON-RPC frame terminator to stdout")?;
    writer.flush().await.context("flushing stdout")
}

async fn forward_with_retry(
    client: &reqwest::Client,
    endpoint: &mut Endpoint,
    endpoint_path: &Path,
    body: &str,
    session: &mut SessionState,
) -> Result<Option<String>> {
    match forward_request(client, endpoint, body, session).await {
        Ok(text) => Ok(text),
        Err(BridgeError::Reauth) => {
            retry_after_reauth(client, endpoint, endpoint_path, body, session).await
        }
        Err(BridgeError::Unreachable(err)) => {
            retry_after_unreachable(client, endpoint, endpoint_path, body, err, session).await
        }
        Err(BridgeError::Other(err)) => Err(err),
    }
}

async fn retry_after_reauth(
    client: &reqwest::Client,
    endpoint: &mut Endpoint,
    endpoint_path: &Path,
    body: &str,
    session: &mut SessionState,
) -> Result<Option<String>> {
    tracing::warn!("mcp-shim: 401 from daemon, re-reading endpoint.toml");
    *endpoint = load_endpoint(endpoint_path)?;
    forward_request(client, endpoint, body, session)
        .await
        .map_err(map_retry_after_reauth_error)
}

fn map_retry_after_reauth_error(err: BridgeError) -> anyhow::Error {
    match err {
        BridgeError::Reauth => anyhow::anyhow!(
            "mcp-shim: 401 from daemon even after re-reading endpoint.toml; \
             daemon may have rotated the token to one this shim cannot read \
             (file mode, ownership)"
        ),
        BridgeError::Unreachable(err) => {
            anyhow::anyhow!("mcp-shim: daemon unreachable on retry after 401: {err}")
        }
        BridgeError::Other(err) => err,
    }
}

async fn retry_after_unreachable(
    client: &reqwest::Client,
    endpoint: &mut Endpoint,
    endpoint_path: &Path,
    body: &str,
    cause: reqwest::Error,
    session: &mut SessionState,
) -> Result<Option<String>> {
    tracing::warn!(error = %cause, "mcp-shim: daemon unreachable, re-reading endpoint.toml");
    *endpoint = load_endpoint(endpoint_path)?;
    forward_request(client, endpoint, body, session)
        .await
        .map_err(map_retry_after_unreachable_error)
}

fn map_retry_after_unreachable_error(err: BridgeError) -> anyhow::Error {
    match err {
        BridgeError::Reauth => anyhow::anyhow!(
            "mcp-shim: 401 on retry after connection failure; \
             daemon may have rotated the token"
        ),
        BridgeError::Unreachable(err) => anyhow::anyhow!(
            "mcp-shim: daemon still unreachable after re-reading \
             endpoint.toml: {err}"
        ),
        BridgeError::Other(err) => err,
    }
}

#[derive(Debug)]
enum BridgeError {
    /// Daemon returned 401. Re-read `endpoint.toml` and retry once.
    Reauth,
    /// Connection-level failure (refused, timeout, dns). Re-read and retry once.
    Unreachable(reqwest::Error),
    /// Anything else: bubble up unchanged.
    Other(anyhow::Error),
}

async fn forward_request(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    body: &str,
    session: &mut SessionState,
) -> Result<Option<String>, BridgeError> {
    let response = send_post(client, endpoint, body, session).await?;
    capture_session_id(&response, session);
    interpret_response(response).await
}

async fn send_post(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    body: &str,
    session: &SessionState,
) -> Result<reqwest::Response, BridgeError> {
    let mut req = client
        .post(&endpoint.url)
        .header(AUTHORIZATION, format!("Bearer {}", endpoint.token))
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .body(body.to_owned());
    if let Some(sid) = session.session_id.as_ref() {
        req = req.header(HeaderName::from_static(MCP_SESSION_HEADER), sid);
    }
    req.send().await.map_err(|err| {
        if err.is_connect() || err.is_timeout() {
            BridgeError::Unreachable(err)
        } else {
            BridgeError::Other(anyhow::Error::from(err))
        }
    })
}

fn capture_session_id(response: &reqwest::Response, session: &mut SessionState) {
    if let Some(value) = response.headers().get(MCP_SESSION_HEADER)
        && session.session_id.as_ref() != Some(value)
    {
        tracing::debug!(
            session_id = ?value,
            "mcp-shim: captured rmcp session id"
        );
        session.session_id = Some(value.clone());
    }
}

async fn interpret_response(response: reqwest::Response) -> Result<Option<String>, BridgeError> {
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(BridgeError::Reauth);
    }
    let status = response.status();
    if status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT {
        // rmcp answers `notifications/*` with 202/204 and no body —
        // there is no JSON-RPC reply to forward to stdout.
        drop(response.bytes().await);
        return Ok(None);
    }
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(BridgeError::Other(anyhow::anyhow!(
            "mcp-shim: daemon returned {status} for POST /mcp: {text}"
        )));
    }
    let content_type = content_type_of(&response);
    let raw = response
        .text()
        .await
        .map_err(|e| BridgeError::Other(anyhow::Error::from(e)))?;
    if raw.is_empty() {
        return Ok(None);
    }
    if content_type.contains("text/event-stream") {
        Ok(Some(extract_first_sse_data(&raw).unwrap_or(raw)))
    } else {
        Ok(Some(raw))
    }
}

fn content_type_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// Pull the first non-empty `data:` payload out of an SSE stream.
///
/// The MCP streamable HTTP transport prefixes each response with one or
/// more keepalive events whose `data:` line is empty; the actual JSON-RPC
/// reply lives in the next `data:` event. We skip empties and return the
/// first payload that carries content. Multi-event streams (subscriptions)
/// are out of scope for this shim — the schema server does not declare
/// `hasResourceSubscribe = true` (ADR-0009 + observed handshake).
fn extract_first_sse_data(sse: &str) -> Option<String> {
    for line in sse.lines() {
        let trimmed = line.trim_end_matches('\r');
        let payload = trimmed
            .strip_prefix("data: ")
            .or_else(|| trimmed.strip_prefix("data:"));
        if let Some(rest) = payload {
            let body = rest.trim();
            if !body.is_empty() {
                return Some(body.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic when the test setup is broken"
    )]

    use super::*;

    #[test]
    fn extract_first_sse_data_picks_first_data_line() {
        let sse = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\n";
        assert_eq!(
            extract_first_sse_data(sse).unwrap(),
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#
        );
    }

    #[test]
    fn extract_first_sse_data_handles_no_space_after_colon() {
        let sse = "data:{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":1}\n";
        assert_eq!(
            extract_first_sse_data(sse).unwrap(),
            r#"{"jsonrpc":"2.0","id":1,"result":1}"#
        );
    }

    #[test]
    fn extract_first_sse_data_returns_none_when_no_data_line() {
        assert!(extract_first_sse_data("event: ping\nretry: 1000\n").is_none());
    }

    #[test]
    fn extract_first_sse_data_skips_empty_keepalive_events() {
        let sse =
            "data: \nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert_eq!(
            extract_first_sse_data(sse).unwrap(),
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#
        );
    }

    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};

    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    /// Build a mock /mcp router that accepts only `expected_token` and
    /// returns a canned JSON body. Used by rotation-recovery tests.
    fn mock_router(expected_token: &'static str, response_body: &'static str) -> Router {
        async fn handler(
            State(state): State<Arc<MockState>>,
            headers: HeaderMap,
            _body: String,
        ) -> impl IntoResponse {
            let auth = headers
                .get(AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "));
            if auth == Some(state.expected_token) {
                (
                    StatusCode::OK,
                    [(CONTENT_TYPE, "application/json")],
                    state.response_body,
                )
                    .into_response()
            } else {
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
            }
        }
        let state = Arc::new(MockState {
            expected_token,
            response_body,
        });
        Router::new().route("/mcp", post(handler)).with_state(state)
    }

    #[derive(Clone)]
    struct MockState {
        expected_token: &'static str,
        response_body: &'static str,
    }

    /// Spawn the router on `127.0.0.1:0`, return its bound URL.
    async fn spawn_mock(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}/mcp")
    }

    fn write_endpoint(dir: &TempDir, url: &str, token: &str) -> PathBuf {
        let path = dir.path().join("endpoint.toml");
        let endpoint = Endpoint {
            version: 1,
            url: url.to_owned(),
            token: token.to_owned(),
            pid: 1234,
            started_at: "2026-04-27T12:00:00+00:00".to_owned(),
        };
        endpoint.write_atomic(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn forward_with_retry_recovers_after_401() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let url = spawn_mock(mock_router("token-B", body)).await;
        let dir = TempDir::new().unwrap();
        let endpoint_path = write_endpoint(&dir, &url, "token-B");
        let mut stale = Endpoint {
            version: 1,
            url: url.clone(),
            token: "token-A-stale".to_owned(),
            pid: 1234,
            started_at: "2026-04-27T11:00:00+00:00".to_owned(),
        };
        let client = build_client().unwrap();
        let mut session = SessionState::default();
        let result = forward_with_retry(&client, &mut stale, &endpoint_path, "{}", &mut session)
            .await
            .unwrap();
        assert_eq!(result.as_deref(), Some(body));
        assert_eq!(stale.token, "token-B");
    }

    #[derive(Clone)]
    struct SessionMockState {
        minted: Arc<StdMutex<Option<String>>>,
    }

    fn session_router() -> Router {
        let state = SessionMockState {
            minted: Arc::new(StdMutex::new(None)),
        };
        Router::new()
            .route("/mcp", post(session_handler))
            .with_state(state)
    }

    async fn session_handler(
        State(state): State<SessionMockState>,
        headers: HeaderMap,
        _body: String,
    ) -> Response {
        let supplied = headers
            .get(MCP_SESSION_HEADER)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let existing = {
            let guard = state.minted.lock().unwrap();
            guard.clone()
        };
        if let Some(existing) = existing {
            return session_followup_response(&existing, supplied.as_deref());
        }
        let sid = "test-session-7777".to_owned();
        *state.minted.lock().unwrap() = Some(sid.clone());
        session_initial_response(&sid)
    }

    fn session_followup_response(existing: &str, supplied: Option<&str>) -> Response {
        if supplied == Some(existing) {
            (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/json")],
                r#"{"jsonrpc":"2.0","id":2,"result":{"ok":"replayed"}}"#,
            )
                .into_response()
        } else {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                [(CONTENT_TYPE, "application/json")],
                r#"{"error":"missing or wrong mcp-session-id"}"#,
            )
                .into_response()
        }
    }

    fn session_initial_response(sid: &str) -> Response {
        let mut head = HeaderMap::new();
        head.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        head.insert(MCP_SESSION_HEADER, sid.parse().unwrap());
        (
            StatusCode::OK,
            head,
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":"first"}}"#,
        )
            .into_response()
    }

    #[tokio::test]
    async fn session_id_is_captured_and_replayed() {
        let url = spawn_mock(session_router()).await;
        let dir = TempDir::new().unwrap();
        let endpoint_path = write_endpoint(&dir, &url, "any");
        let mut endpoint = Endpoint {
            version: 1,
            url: url.clone(),
            token: "any".to_owned(),
            pid: 1,
            started_at: "2026-04-27T12:00:00+00:00".to_owned(),
        };
        let client = build_client().unwrap();
        let mut session = SessionState::default();
        let first = forward_with_retry(&client, &mut endpoint, &endpoint_path, "{}", &mut session)
            .await
            .unwrap();
        assert!(first.unwrap().contains(r#""ok":"first""#));
        assert!(session.session_id.is_some(), "first call must capture sid");
        let second = forward_with_retry(&client, &mut endpoint, &endpoint_path, "{}", &mut session)
            .await
            .unwrap();
        assert!(
            second.unwrap().contains(r#""ok":"replayed""#),
            "second call must replay the captured sid and succeed"
        );
    }

    #[tokio::test]
    async fn forward_with_retry_recovers_after_econnrefused() {
        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#;
        let live_url = spawn_mock(mock_router("token-A", body)).await;
        let bad_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad_listener.local_addr().unwrap();
        drop(bad_listener);
        let stale_url = format!("http://{bad_addr}/mcp");
        let dir = TempDir::new().unwrap();
        let endpoint_path = write_endpoint(&dir, &live_url, "token-A");
        let mut stale = Endpoint {
            version: 1,
            url: stale_url,
            token: "token-A".to_owned(),
            pid: 1234,
            started_at: "2026-04-27T11:00:00+00:00".to_owned(),
        };
        let client = build_client().unwrap();
        let mut session = SessionState::default();
        let result = forward_with_retry(&client, &mut stale, &endpoint_path, "{}", &mut session)
            .await
            .unwrap();
        assert_eq!(result.as_deref(), Some(body));
        assert_eq!(stale.url, live_url);
    }
}
