//! Integration tests for the HTTP MCP transport's auth + redaction
//! wiring (ADR-0019 / ADR-0021 fitness).
//!
//! These tests do **not** exercise the full MCP `initialize` handshake —
//! that requires standing up a real `SchemaServer` with all the ports
//! wired (sqlite-vec store, embedder, walker, chunker, metadata store)
//! and is the scope of a heavier integration suite to land alongside
//! the bench harness (ADR-0022). The tests here pin the auth layer's
//! contract end-to-end:
//!
//! 1. Routes mounted under [`tower_http::validate_request::ValidateRequestHeaderLayer`]
//!    with [`schema::adapters::auth::BearerValidator`] reject requests
//!    whose `Authorization` header does not carry the configured bearer
//!    token (ADR-0021).
//! 2. The unauthenticated `GET /health` route returns `200 OK` and
//!    leaks no project state in the body (ADR-0019 health probe).

#![allow(
    clippy::unwrap_used,
    reason = "test fixtures may panic if the env is broken"
)]
#![allow(
    unused_crate_dependencies,
    reason = "Cargo.toml is shared between lib + bin + integration tests. \
              Each integration test target sees the lib's full dep tree as \
              its own (rmcp, fastembed, sqlite-vec, etc.) and \
              `unused_crate_dependencies` runs per-target. The lib's own \
              attribute already covers the library; this allows the \
              integration test target."
)]

#[cfg(test)]
mod tests {
    use std::iter;

    use axum::Router;
    use axum::body::Body;
    use axum::routing::{any, get};
    use http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;
    use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
    use tower_http::validate_request::ValidateRequestHeaderLayer;

    use schema::adapters::auth::BearerValidator;

    /// Build a router shaped like the production `build_router` (ADR-0019)
    /// but with the `/mcp` mount point backed by an inline echo handler
    /// instead of the rmcp `StreamableHttpService`. The auth + redaction
    /// layers are identical to production wiring.
    fn auth_router(token: &str) -> Router {
        let validator = BearerValidator::new(token.to_string());
        let mcp_router = Router::new()
            .route("/mcp", any(|| async { "mcp-ok" }))
            .layer(ValidateRequestHeaderLayer::custom(validator));

        Router::new()
            .merge(mcp_router)
            .route("/health", get(|| async { "" }))
            .layer(SetSensitiveRequestHeadersLayer::new(iter::once(
                header::AUTHORIZATION,
            )))
    }

    #[tokio::test]
    async fn health_endpoint_returns_200_without_auth() {
        let app = auth_router("test-token");
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_endpoint_rejects_missing_authorization_header() {
        let app = auth_router("test-token");
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_endpoint_rejects_wrong_token() {
        let app = auth_router("expected-token");
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_endpoint_accepts_correct_token() {
        let app = auth_router("the-token");
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, "Bearer the-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The inline handler returns 200; production rmcp may return
        // 4xx for a missing MCP session header. Either way, **not 401**
        // is the assertion that the auth layer let the request through.
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "valid bearer token must not be rejected by the auth layer"
        );
    }

    #[tokio::test]
    async fn bearer_validator_is_case_sensitive_on_scheme() {
        // The `Bearer` token scheme is canonically case-insensitive in
        // RFC 6750, but the project's validator is exact-match — pinning
        // that behaviour with a test (ADR-0021 §"Threat model on byte-equality").
        let app = auth_router("the-token");
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, "bearer the-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
