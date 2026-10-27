use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use analysis_cache_proto::skycache::v1::analysis_cache_server::AnalysisCacheServer;
use clap::Parser;
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use skycache::db::{CredentialSource, DbEndpoint, DbSslMode};
use skycache::oidc::{OidcVerifier, parse_issuer_audience};
use skycache::service::Service;
use skycache::store::Store;
use skycache::tenant::{AuthenticationLayer, DEFAULT_TENANT_CLAIM};
use tonic::server::NamedService;
use tonic::transport::Server;
use tower::ServiceBuilder;

/// Must match the client's ceiling: a full read batch is well over gRPC's 4 MiB default, and
/// exceeding it fails the whole batch rather than degrading to a miss.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "skycache")]
struct Args {
    #[arg(long, env = "SKYCACHE_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,

    /// Trusted OIDC issuers. A token's `iss` selects one; a token naming none is refused.
    #[arg(
        long = "oidc-issuer",
        env = "SKYCACHE_OIDC_ISSUERS",
        value_delimiter = ','
    )]
    oidc_issuers: Vec<String>,

    /// Per-issuer audience pins, as `issuer=audience`. Repeatable.
    #[arg(long = "oidc-issuer-audience", value_parser = parse_issuer_audience)]
    oidc_issuer_audiences: Vec<(String, String)>,

    /// Audience applied to any issuer with no pin of its own.
    #[arg(long, env = "SKYCACHE_OIDC_AUDIENCE")]
    oidc_audience: Option<String>,

    /// Claim carrying the tenant UUID.
    #[arg(long, env = "SKYCACHE_TENANT_CLAIM", default_value = DEFAULT_TENANT_CLAIM)]
    tenant_claim: String,

    /// Acknowledges that cached analysis is not invalidated when its inputs change.
    ///
    /// Required to start. Serving a cache whose entries can outlive the sources they were derived
    /// from produces wrong builds, not slow ones, so the operator says so explicitly rather than
    /// inheriting it from a default.
    #[arg(long, env = "SKYCACHE_ACCEPT_UNSOUND_INVALIDATION")]
    accept_unsound_invalidation: bool,

    /// `host` or `host:port`.
    #[arg(long, env = "SKYCACHE_DB_ENDPOINT")]
    db_endpoint: String,

    #[arg(long, env = "SKYCACHE_DB_USER", default_value = "skycache")]
    db_user: String,

    #[arg(long, env = "SKYCACHE_DB_NAME", default_value = "skycache")]
    db_name: String,

    #[arg(long, env = "SKYCACHE_DB_SSL_MODE", default_value = "require")]
    db_ssl_mode: DbSslMode,

    /// Read on every connect. Without it the password comes from `PGPASSWORD`.
    #[arg(long, env = "SKYCACHE_DB_PASSWORD_FILE")]
    db_password_file: Option<PathBuf>,

    #[arg(long, env = "SKYCACHE_POOL_SIZE", default_value_t = 16)]
    pool_size: u32,

    #[arg(long, env = "SKYCACHE_HEALTH_INTERVAL_S", default_value_t = 10)]
    health_interval_s: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !args.accept_unsound_invalidation {
        return Err(miette!(
            "refusing to start without --accept-unsound-invalidation: cached analysis is not \
             invalidated when its inputs change, so entries can be served after the sources they \
             were derived from have moved"
        ));
    }

    skycache::telemetry::install("skycache");
    skycache::metrics::initialize();

    let verifier = Arc::new(
        OidcVerifier::from_cli(
            &args.oidc_issuers,
            &args.oidc_issuer_audiences,
            args.oidc_audience.as_deref(),
        )
        .await?,
    );
    tracing::info!(issuers = args.oidc_issuers.len(), "trusting OIDC issuers");

    let endpoint = DbEndpoint::from_flags(
        &args.db_endpoint,
        args.db_user,
        args.db_name,
        args.db_ssl_mode,
    )?;
    let credentials = args
        .db_password_file
        .map_or(CredentialSource::Env, CredentialSource::File);
    let store = Store::connect(endpoint, credentials, args.pool_size)
        .await
        .wrap_err("connecting to Postgres")?;

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    tokio::spawn(skycache::health_gate(
        health_reporter,
        store.clone(),
        <AnalysisCacheServer<Service> as NamedService>::NAME,
        Duration::from_secs(args.health_interval_s.max(1)),
    ));

    let cache = AnalysisCacheServer::new(Service::new(store))
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);

    tracing::info!(listen = %args.listen, "skycache listening");
    // The layer wraps the cache service alone, not the server: the health service is what the
    // kubelet probes, it carries no tenant data, and a probe has no token to present.
    let authenticated = ServiceBuilder::new()
        .layer(AuthenticationLayer::new(verifier, args.tenant_claim))
        .service(cache);

    let result = Server::builder()
        .tcp_nodelay(true)
        .add_service(health_service)
        .add_service(authenticated)
        .serve_with_shutdown(args.listen, shutdown_signal())
        .await
        .into_diagnostic()
        .wrap_err("serving the analysis cache");
    skycache::telemetry::shutdown();
    result
}

async fn shutdown_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = term => {},
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::error!(%error, "failed to listen for ctrl-c");
            }
        }
    }
}
