//! A pool of diesel-async Postgres connections over rustls.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bb8::ManageConnection;
use diesel::IntoSql;
use diesel::sql_types::Integer;
use diesel_async::pooled_connection::PoolableConnection;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

pub type DbPool = bb8::Pool<ConnectionManager>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbSslMode {
    Disable,
    Prefer,
    Require,
}

impl FromStr for DbSslMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "disable" => Ok(Self::Disable),
            "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            other => Err(format!(
                "unknown ssl mode {other:?}; expected disable, prefer or require"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DbEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub dbname: String,
    pub ssl_mode: DbSslMode,
}

impl DbEndpoint {
    /// `host_port` is `host` or `host:port`; bare IPv6 literals are not supported.
    pub fn from_flags(
        host_port: &str,
        user: String,
        dbname: String,
        ssl_mode: DbSslMode,
    ) -> Result<Self> {
        let (host, port) = match host_port.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse::<u16>()
                    .into_diagnostic()
                    .wrap_err_with(|| format!("invalid port in db endpoint {host_port:?}"))?,
            ),
            None => (host_port.to_string(), 5432),
        };
        if host.is_empty() {
            return Err(miette!("the db endpoint must name a host"));
        }
        Ok(Self {
            host,
            port,
            user,
            dbname,
            ssl_mode,
        })
    }
}

#[derive(Clone, Debug)]
pub enum CredentialSource {
    Static(String),
    /// `PGPASSWORD`, read at connect time; tokio-postgres does not read it itself.
    Env,
    /// Re-read on every connect, so a rotated Kubernetes Secret applies without a restart.
    File(PathBuf),
}

impl CredentialSource {
    async fn password(&self) -> Result<Option<String>> {
        match self {
            Self::Static(password) => Ok(Some(password.clone())),
            Self::Env => Ok(std::env::var("PGPASSWORD").ok()),
            Self::File(path) => tokio::fs::read_to_string(path)
                .await
                .into_diagnostic()
                .wrap_err_with(|| format!("reading the database password from {}", path.display()))
                .map(|password| Some(password.trim_end_matches(['\r', '\n']).to_string())),
        }
    }
}

/// Lazy: no connection opens until the first checkout.
pub async fn new_pool(
    endpoint: DbEndpoint,
    credentials: CredentialSource,
    max_size: u32,
) -> Result<DbPool> {
    bb8::Pool::builder()
        .max_size(max_size)
        .build(ConnectionManager {
            endpoint,
            credentials,
            tls: rustls_connector()?,
        })
        .await
        .map_err(|e| miette!("building the Postgres connection pool: {e}"))
}

fn rustls_connector() -> Result<MakeRustlsConnect> {
    // An explicit provider: the process default panics at first use when two backends are linked.
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .into_diagnostic()
            .wrap_err("building the Postgres TLS config")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

pub struct ConnectionManager {
    endpoint: DbEndpoint,
    credentials: CredentialSource,
    tls: MakeRustlsConnect,
}

#[derive(Debug)]
pub struct ConnectError(String);

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnectError {}

impl ConnectionManager {
    async fn try_connect(&self) -> Result<AsyncPgConnection> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.endpoint.host)
            .port(self.endpoint.port)
            .user(&self.endpoint.user)
            .dbname(&self.endpoint.dbname)
            .ssl_mode(match self.endpoint.ssl_mode {
                DbSslMode::Disable => SslMode::Disable,
                DbSslMode::Prefer => SslMode::Prefer,
                DbSslMode::Require => SslMode::Require,
            });
        if let Some(password) = self.credentials.password().await? {
            config.password(password);
        }
        let (client, connection) = config
            .connect(self.tls.clone())
            .await
            .into_diagnostic()
            .wrap_err("connecting to Postgres")?;
        AsyncPgConnection::try_from_client_and_connection(client, connection)
            .await
            .into_diagnostic()
            .wrap_err("establishing the diesel connection")
    }
}

impl ManageConnection for ConnectionManager {
    type Connection = AsyncPgConnection;
    type Error = ConnectError;

    async fn connect(&self) -> std::result::Result<Self::Connection, Self::Error> {
        tokio::time::timeout(CONNECT_TIMEOUT, self.try_connect())
            .await
            .map_err(|_| ConnectError("the Postgres connection attempt timed out".to_string()))?
            .map_err(|e| ConnectError(format!("{e:?}")))
    }

    async fn is_valid(&self, conn: &mut Self::Connection) -> std::result::Result<(), Self::Error> {
        diesel::select(1_i32.into_sql::<Integer>())
            .execute(conn)
            .await
            .map(|_| ())
            .map_err(|e| ConnectError(format!("the connection health check failed: {e}")))
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        // SELECT 1 succeeds inside an abandoned transaction, so it cannot decide reuse.
        std::thread::panicking() || conn.is_broken()
    }
}

/// Encrypts without authenticating the server: a self-managed Postgres certificate has no stable
/// identity to pin, so confidentiality is the goal.
#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
