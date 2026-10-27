//! Skycache: stores Bazel's serialized Skyframe analysis graph so a cold workspace can download it
//! instead of re-analyzing.
//!
//! EXPERIMENTAL. Invalidation is not implemented on the Bazel side: the client reports every source
//! path as unchanged, so an entry stays a hit even after the files it was derived from move. That
//! is a property of this build rather than of any one caller, so the binary refuses to start
//! without `--accept-unsound-invalidation` — see [`main`](../skycache/index.html).

pub mod db;
pub mod metrics;
pub mod oidc;
pub mod service;
pub mod store;
pub mod telemetry;
pub mod tenant;

use std::time::Duration;

use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;
use tracing::warn;

use crate::store::Store;

/// Reports the service unserving whenever the database is unreachable.
///
/// Returned as a future for the caller to spawn. Without it the pod answers readiness from the
/// listening socket alone and keeps taking traffic it cannot serve.
pub async fn health_gate(
    reporter: HealthReporter,
    store: Store,
    service_name: &'static str,
    interval: Duration,
) {
    let mut serving = None;
    loop {
        let healthy = match store.ping().await {
            Ok(()) => true,
            Err(error) => {
                warn!(error = ?error, "database health check failed");
                // Re-apply the schema before giving up. `migrate` is idempotent and the store is a
                // cache, so re-creating a table that vanished costs a cold cache rather than data:
                // the alternative is a pod that stays broken until someone restarts it. It also
                // recovers the startup race where the first migration ran against a Postgres that
                // was still initializing.
                match store.migrate().await {
                    Ok(()) => store.ping().await.is_ok(),
                    Err(error) => {
                        warn!(error = ?error, "re-applying the schema failed");
                        false
                    }
                }
            }
        };
        if serving != Some(healthy) {
            let status = if healthy {
                ServingStatus::Serving
            } else {
                ServingStatus::NotServing
            };
            reporter.set_service_status(service_name, status).await;
            serving = Some(healthy);
        }
        tokio::time::sleep(interval).await;
    }
}
