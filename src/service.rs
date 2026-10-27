//! The gRPC surface.

use analysis_cache_proto::skycache::v1::analysis_cache_server::AnalysisCache;
use analysis_cache_proto::skycache::v1::{
    Entry, GetRequest, GetResponse, PutRequest, PutResponse,
};
use tonic::{Request, Response, Status};
use tracing::warn;

use crate::metrics;
use crate::store::{Store, StoredEntry};
use crate::tenant::TenantId;

pub struct Service {
    store: Store,
}

impl Service {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

/// The tenant [`crate::tenant::ResolveTenant`] attached.
///
/// Its absence is a wiring fault — the service was mounted without the interceptor — so this fails
/// the request rather than picking a tenant. Internal, not a client error: nothing the caller sent
/// could have prevented it.
fn tenant_of<T>(request: &Request<T>) -> Result<TenantId, Status> {
    request
        .extensions()
        .get::<TenantId>()
        .copied()
        .ok_or_else(|| {
            metrics::record_rejected("unattributed_request");
            warn!("a request reached the service with no resolved tenant");
            Status::internal("the request was not attributed to a tenant")
        })
}

#[tonic::async_trait]
impl AnalysisCache for Service {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let TenantId(tenant) = tenant_of(&request)?;
        let request = request.into_inner();

        let found = self.store.get(tenant, &request.keys).await.map_err(|e| {
            warn!(error = ?e, "reading entries failed");
            Status::internal("reading entries failed")
        })?;

        let bytes: usize = found.iter().map(|e| e.value.len()).sum();
        metrics::record_get(tenant, request.keys.len(), found.len(), bytes);

        let entries = found
            .into_iter()
            .map(|entry| Entry {
                key: entry.key,
                value: entry.value,
            })
            .collect();
        Ok(Response::new(GetResponse { entries }))
    }

    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let TenantId(tenant) = tenant_of(&request)?;
        let request = request.into_inner();

        let offered = request.entries.len();
        let bytes: usize = request.entries.iter().map(|e| e.value.len()).sum();
        let entries: Vec<StoredEntry> = request
            .entries
            .into_iter()
            .map(|e| StoredEntry {
                key: e.key,
                value: e.value,
            })
            .collect();

        let stored = self.store.put(tenant, &entries).await.map_err(|e| {
            warn!(error = ?e, "writing entries failed");
            Status::internal("writing entries failed")
        })?;

        metrics::record_put(tenant, offered, stored, bytes);
        Ok(Response::new(PutResponse {
            stored: stored as i64,
        }))
    }
}

#[cfg(test)]
mod tests {
    use analysis_cache_proto::skycache::v1::GetRequest;
    use tonic::{Code, Request};

    use super::tenant_of;

    #[test]
    fn an_unattributed_request_fails_rather_than_choosing_a_tenant() {
        let request = Request::new(GetRequest { keys: Vec::new() });
        let Err(status) = tenant_of(&request) else {
            panic!("an unattributed request was accepted");
        };
        assert_eq!(status.code(), Code::Internal);
    }
}
