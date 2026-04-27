//! Bearer-token validator for the HTTP MCP transport (ADR-0021).
//!
//! `tower-http` 0.6's `ValidateRequestHeaderLayer` exposes only `accept` and
//! `custom` constructors; the `bearer` constructor referenced in earlier ADR
//! drafts does not exist. This module implements `ValidateRequest` against a
//! constant token, then mounts under `ValidateRequestHeaderLayer::custom(...)`.
//!
//! `ResponseBody` is `axum::body::Body` rather than `http_body_util::Full<Bytes>`
//! because axum's `Router::nest_service` homogenises the inner service body to
//! `axum::body::Body`; a `tower-http` validator layered above must return the
//! same type so the chain types check.

use axum::body::Body;
use http::{Request, Response, StatusCode, header};
use tower_http::validate_request::ValidateRequest;

/// Validates `Authorization: Bearer <token>` against a constant string.
///
/// Cloned per request by tower; the inner `String` is wrapped in a
/// reference-counted-only-when-cloned shape (here just a plain `String` —
/// per-request clone of a 36-byte UUID is cheap; if profiling shows it,
/// switch to `Arc<str>` later).
#[derive(Clone, Debug)]
pub struct BearerValidator {
    expected: String,
}

impl BearerValidator {
    #[must_use]
    pub const fn new(expected: String) -> Self {
        Self { expected }
    }
}

impl<B> ValidateRequest<B> for BearerValidator {
    type ResponseBody = Body;

    fn validate(&mut self, request: &mut Request<B>) -> Result<(), Response<Self::ResponseBody>> {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        if supplied.is_some_and(|s| s == self.expected) {
            Ok(())
        } else {
            // `Response::new(Body::empty())` is infallible; mutate status
            // afterwards rather than going through `Response::builder()`
            // (whose `body()` returns `Result` for HeaderValue parse cases
            // we never trigger).
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            Err(response)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use http::{Method, Request, header};
    use tower_http::validate_request::ValidateRequest;

    use super::BearerValidator;

    fn req_with(authorization: Option<&str>) -> Request<()> {
        let mut builder = Request::builder().method(Method::POST).uri("/mcp");
        if let Some(value) = authorization {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn accepts_matching_bearer() {
        let mut validator = BearerValidator::new("secret-token".to_string());
        let mut req = req_with(Some("Bearer secret-token"));
        assert!(validator.validate(&mut req).is_ok());
    }

    #[test]
    fn rejects_missing_authorization_header() {
        let mut validator = BearerValidator::new("secret-token".to_string());
        let mut req = req_with(None);
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rejects_wrong_token() {
        let mut validator = BearerValidator::new("secret-token".to_string());
        let mut req = req_with(Some("Bearer other-token"));
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rejects_basic_authorization_scheme() {
        let mut validator = BearerValidator::new("secret-token".to_string());
        let mut req = req_with(Some("Basic dXNlcjpwYXNz"));
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rejects_bearer_with_wrong_prefix_case() {
        // Bearer scheme is canonically case-insensitive, but the simple-string
        // matcher here is exact. Document the choice with a test rather than
        // pretending to honour the full spec.
        let mut validator = BearerValidator::new("secret-token".to_string());
        let mut req = req_with(Some("bearer secret-token"));
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }
}
