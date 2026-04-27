//! Bearer-token validator for the HTTP MCP transport (ADR-0021, amended by ADR-0026).
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
//!
//! Two validators coexist:
//!
//! - [`BearerValidator`] — single-token (one project per process), retained
//!   for the ADR-0019 / ADR-0020 per-project daemon path that ships in
//!   production today.
//! - [`MultiTenantBearerValidator`] — `token → ProjectId` map, the shape
//!   ADR-0026 amends ADR-0021 into. On accept, the resolved [`ProjectId`]
//!   is inserted into the request's extensions so the downstream router
//!   and MCP handlers can dispatch to the matching `ProjectInstance`. The
//!   URL-vs-token mismatch defence-in-depth check (request URL declares
//!   one `project_id`, bearer resolves to another) lives in a separate
//!   middleware that runs after the router has extracted the path param;
//!   this validator stays narrow on `(token → ProjectId)` resolution.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use axum::body::Body;
use http::{Request, Response, StatusCode, header};
use tower_http::validate_request::ValidateRequest;

use crate::adapters::project_identity::ProjectId;

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

// ─── Multi-tenant validator (ADR-0026 amendment of ADR-0021) ─────────────────

/// Shared, mutable token → `ProjectId` map.
///
/// The shared daemon reads this on every request and writes to it via
/// [`ProjectTokenRegistry::insert`] / [`ProjectTokenRegistry::remove`]
/// when a project is registered or unregistered.
///
/// `Arc<RwLock<HashMap<...>>>` is the simplest shape that lets the validator
/// be cheaply cloned per request while the daemon mutates the map elsewhere.
/// Reads are short (one `HashMap::get`); writes happen at registration time
/// (rare). Lock contention is not a concern at expected request rates.
///
/// Lock poisoning recovers via [`PoisonError::into_inner`] rather than
/// panicking — the registry holds only owned `String` / `ProjectId` values,
/// which cannot be left in an inconsistent state by a mid-write panic, so
/// continuing with the inner map is strictly safer than crashing the
/// daemon for every other registered project.
#[derive(Clone, Debug, Default)]
pub struct ProjectTokenRegistry {
    inner: Arc<RwLock<HashMap<String, ProjectId>>>,
}

impl ProjectTokenRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read_guard(&self) -> RwLockReadGuard<'_, HashMap<String, ProjectId>> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_guard(&self) -> RwLockWriteGuard<'_, HashMap<String, ProjectId>> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register a `(token, project_id)` pair. Replaces any existing token
    /// binding (a re-registration of the same project rotates its token).
    pub fn insert(&self, token: String, project_id: ProjectId) {
        self.write_guard().insert(token, project_id);
    }

    /// Remove a token binding. Returns the `ProjectId` previously bound, or
    /// `None` if the token was unknown.
    #[must_use = "the removed binding may be inspected to confirm a prior registration"]
    pub fn remove(&self, token: &str) -> Option<ProjectId> {
        self.write_guard().remove(token)
    }

    /// Resolve a token to its `ProjectId`. Returns `None` for unknown
    /// tokens. Used by [`MultiTenantBearerValidator::validate`].
    #[must_use]
    pub fn lookup(&self, token: &str) -> Option<ProjectId> {
        self.read_guard().get(token).cloned()
    }

    /// Number of registered tokens. Used in tests and `schema project list`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read_guard().len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read_guard().is_empty()
    }
}

/// Multi-tenant bearer validator.
///
/// Resolves the request's bearer token to a [`ProjectId`] via the shared
/// [`ProjectTokenRegistry`] and injects the resolved id into the request's
/// extensions so downstream layers (router, MCP handlers) can dispatch to
/// the matching `ProjectInstance`.
///
/// On unknown / missing / wrong-scheme bearer, returns 401 with an empty
/// body (same shape as [`BearerValidator`]).
#[derive(Clone, Debug)]
pub struct MultiTenantBearerValidator {
    registry: ProjectTokenRegistry,
}

impl MultiTenantBearerValidator {
    #[must_use]
    pub const fn new(registry: ProjectTokenRegistry) -> Self {
        Self { registry }
    }
}

impl<B> ValidateRequest<B> for MultiTenantBearerValidator {
    type ResponseBody = Body;

    #[expect(
        clippy::option_if_let_else,
        reason = "the `map_or_else` rewrite exposes `Result<(), Response<Body>>` at the closure boundary, which trips `result_large_err`; the if-let-else body matches the shape of the single-token `BearerValidator::validate` above and stays under the size lint"
    )]
    fn validate(&mut self, request: &mut Request<B>) -> Result<(), Response<Self::ResponseBody>> {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        let project_id = supplied.and_then(|token| self.registry.lookup(token));

        if let Some(id) = project_id {
            // Downstream layers read this with
            // `request.extensions().get::<ProjectId>()`. Inserting on the
            // success path means a 401 leaves no extension behind, so any
            // handler that observes the extension can assume an
            // authenticated request.
            request.extensions_mut().insert(id);
            Ok(())
        } else {
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

    // ─── MultiTenantBearerValidator (ADR-0026) ───────────────────────────

    use std::path::PathBuf;

    use super::{MultiTenantBearerValidator, ProjectTokenRegistry};
    use crate::adapters::project_identity::ProjectId;

    fn project_id(name: &str) -> ProjectId {
        ProjectId::new(name, &PathBuf::from(format!("/tmp/{name}")))
    }

    fn registry_with(pairs: &[(&str, &str)]) -> ProjectTokenRegistry {
        let registry = ProjectTokenRegistry::new();
        for (token, name) in pairs {
            registry.insert((*token).to_string(), project_id(name));
        }
        registry
    }

    #[test]
    fn multi_tenant_accepts_known_token_and_injects_project_id() {
        let registry = registry_with(&[("alpha-token", "alpha"), ("beta-token", "beta")]);
        let mut validator = MultiTenantBearerValidator::new(registry);

        let mut req = req_with(Some("Bearer alpha-token"));
        validator.validate(&mut req).unwrap();

        let resolved = req.extensions().get::<ProjectId>().unwrap();
        assert_eq!(resolved, &project_id("alpha"));
    }

    #[test]
    fn multi_tenant_rejects_unknown_token() {
        let registry = registry_with(&[("alpha-token", "alpha")]);
        let mut validator = MultiTenantBearerValidator::new(registry);

        let mut req = req_with(Some("Bearer not-registered"));
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
        assert!(req.extensions().get::<ProjectId>().is_none());
    }

    #[test]
    fn multi_tenant_rejects_missing_authorization_header() {
        let registry = registry_with(&[("alpha-token", "alpha")]);
        let mut validator = MultiTenantBearerValidator::new(registry);

        let mut req = req_with(None);
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn multi_tenant_rejects_basic_authorization_scheme() {
        let registry = registry_with(&[("alpha-token", "alpha")]);
        let mut validator = MultiTenantBearerValidator::new(registry);

        let mut req = req_with(Some("Basic YWxwaGEtdG9rZW46"));
        let resp = validator.validate(&mut req).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn multi_tenant_dispatches_each_token_to_its_own_project() {
        let registry = registry_with(&[("alpha-token", "alpha"), ("beta-token", "beta")]);
        let mut validator = MultiTenantBearerValidator::new(registry);

        let mut alpha_req = req_with(Some("Bearer alpha-token"));
        validator.validate(&mut alpha_req).unwrap();
        let alpha_id = alpha_req.extensions().get::<ProjectId>().unwrap().clone();

        let mut beta_req = req_with(Some("Bearer beta-token"));
        validator.validate(&mut beta_req).unwrap();
        let beta_id = beta_req.extensions().get::<ProjectId>().unwrap().clone();

        assert_eq!(alpha_id, project_id("alpha"));
        assert_eq!(beta_id, project_id("beta"));
        assert_ne!(alpha_id, beta_id);
    }

    #[test]
    fn multi_tenant_observes_runtime_registration_and_revocation() {
        // Daemon flow: insert at register, remove at unregister, lookups
        // must reflect the live state without reconstructing the validator.
        let registry = ProjectTokenRegistry::new();
        let mut validator = MultiTenantBearerValidator::new(registry.clone());

        // Empty registry → 401 even for plausible-looking tokens.
        let mut early = req_with(Some("Bearer alpha-token"));
        let resp = validator.validate(&mut early).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());

        // Register → next request resolves.
        registry.insert("alpha-token".to_string(), project_id("alpha"));
        assert_eq!(registry.len(), 1);
        let mut after_register = req_with(Some("Bearer alpha-token"));
        validator.validate(&mut after_register).unwrap();

        // Revoke → subsequent request rejected; previous request keeps
        // its extension (unaffected by later revocation).
        let removed = registry.remove("alpha-token");
        assert_eq!(removed, Some(project_id("alpha")));
        assert!(registry.is_empty());

        let mut after_revoke = req_with(Some("Bearer alpha-token"));
        let resp = validator.validate(&mut after_revoke).unwrap_err();
        assert_eq!(resp.status(), http::StatusCode::UNAUTHORIZED);
    }
}
