//! Resolving the tenant a request is served under.
//!
//! The tenant is never on the wire. Bazel attaches a bearer token from its credential helper, and
//! this layer verifies that token against the trusted OIDC issuers and reads the tenant from its
//! claims. A forged token cannot pass, so the tenant is proven here rather than asserted by the
//! caller. There is no single-tenant mode: a token without a usable tenant claim is refused.

use core::convert::Infallible;
use core::task::{Context, Poll};
use std::sync::Arc;

use axum::body::Body as AxumBody;
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt};
use http::{Request, Response};
use hyper::body::Body;
use tonic::Status;
use uuid::Uuid;

use crate::metrics;
use crate::oidc::{OidcVerifier, TenantClaim, parse_bearer};

/// The tenant a request is served under.
///
/// Reaches a handler only through [`AuthenticationLayer`], so a request that was never attributed
/// cannot be served: there is no default tenant and no unscoped fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TenantId(pub Uuid);

/// The claim read for the tenant UUID unless `--tenant-claim` names another.
pub const DEFAULT_TENANT_CLAIM: &str = "tenantId";

struct Authentication {
    verifier: Arc<OidcVerifier>,
    tenant_claim: String,
}

/// Verifies the bearer token and attaches the tenant it proves.
#[derive(Clone)]
pub struct AuthenticationLayer {
    authentication: Arc<Authentication>,
}

impl AuthenticationLayer {
    #[must_use]
    pub fn new(verifier: Arc<OidcVerifier>, tenant_claim: String) -> Self {
        Self {
            authentication: Arc::new(Authentication {
                verifier,
                tenant_claim,
            }),
        }
    }
}

impl<S> tower::Layer<S> for AuthenticationLayer {
    type Service = AuthenticationMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthenticationMiddleware {
            inner,
            authentication: Arc::clone(&self.authentication),
        }
    }
}

#[derive(Clone)]
pub struct AuthenticationMiddleware<S> {
    inner: S,
    authentication: Arc<Authentication>,
}

impl<S, ReqBody, ResBody> tower::Service<Request<ReqBody>> for AuthenticationMiddleware<S>
where
    S: tower::Service<Request<ReqBody>, Response = Response<ResBody>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Body<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
{
    type Response = Response<AxumBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        // Clone-and-swap: the readied clone is the one that must be called, and `self.inner` keeps
        // a fresh one for the next request.
        let replacement = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, replacement);
        let authentication = Arc::clone(&self.authentication);
        async move {
            let bearer = request
                .headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_bearer)
                .map(str::to_owned);
            match resolve_tenant(&authentication, bearer.as_deref()).await {
                Ok(tenant) => {
                    request.extensions_mut().insert(TenantId(tenant));
                }
                Err(status) => return Ok(status.into_http::<AxumBody>()),
            }
            inner
                .call(request)
                .await
                .map(|response| response.map(AxumBody::new))
        }
        .boxed()
    }
}

/// Keeps the gRPC service name so tonic's router still matches through the layer.
impl<S: tonic::server::NamedService> tonic::server::NamedService for AuthenticationMiddleware<S> {
    const NAME: &'static str = S::NAME;
}

/// Verifies `bearer` and returns the tenant it proves.
///
/// Statuses are deliberately uninformative about why a token failed: a caller learns that theirs
/// was refused, not which issuers or tenants exist.
async fn resolve_tenant(
    authentication: &Authentication,
    bearer: Option<&str>,
) -> Result<Uuid, Status> {
    let Some(bearer) = bearer else {
        metrics::record_rejected("no_bearer");
        return Err(Status::unauthenticated(
            "the request carried no bearer token",
        ));
    };
    let claims = authentication
        .verifier
        .verify(bearer)
        .await
        .map_err(|error| {
            tracing::debug!(?error, "bearer token refused");
            metrics::record_rejected("unverifiable_bearer");
            Status::unauthenticated("the bearer token could not be verified")
        })?;
    let tenant = match claims.tenant_claim(&authentication.tenant_claim) {
        TenantClaim::Value(tenant) => Uuid::parse_str(tenant).ok(),
        // A present-but-malformed claim is refused rather than read as absent, so a wrong-typed
        // token never resolves to some other tenant.
        TenantClaim::Absent | TenantClaim::NotString => None,
    };
    tenant.ok_or_else(|| {
        metrics::record_rejected("no_tenant_claim");
        Status::permission_denied("the token carries no usable tenant claim")
    })
}
