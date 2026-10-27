//! The gRPC surface over a real server, including the status codes a client branches on.
//!
//! Every case goes through the authentication layer against a mock OIDC issuer, because that layer
//! is the only thing standing between one tenant's entries and another's: a test that constructed
//! the service directly would prove nothing about what a client can reach. What is pinned here is
//! which tenant a verified token resolves to, what happens when it resolves to none, and that a
//! token from an untrusted issuer is refused.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use analysis_cache_proto::skycache::v1::analysis_cache_client::AnalysisCacheClient;
use analysis_cache_proto::skycache::v1::analysis_cache_server::AnalysisCacheServer;
use analysis_cache_proto::skycache::v1::{Entry, GetRequest, PutRequest};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use miette::{IntoDiagnostic, Result, miette};
use serde_json::json;
use skycache::db::{CredentialSource, DbEndpoint, DbSslMode, new_pool};
use skycache::oidc::OidcVerifier;
use skycache::service::Service;
use skycache::store::Store;
use skycache::tenant::{AuthenticationLayer, DEFAULT_TENANT_CLAIM};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Server};
use tonic::{Code, Request};
use tower::ServiceBuilder;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACME: &str = "11111111-1111-1111-1111-111111111111";
const BETA: &str = "22222222-2222-2222-2222-222222222222";
const KID: &str = "test-key";

/// A fixed test RSA-2048 private key (PKCS#1 PEM); its public half is [`public_jwk`].
const PRIVATE_KEY_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEA0qyCnHMYukYoLXQicPZoz+iup1lKSfBWK5cedbDLmjW8GQeD
xlYA2ARK7M2XwOd8z6xj4MUInAT1Fu6ezoNScSsCbDNYkDyxuQOtThFMEAUvhWfD
qxTQy0fV88viudKVFQ6qxaKDF39pfs4VBgBE9z2pi0gaqYQBeR7MujYmUmkKuKKY
+dnEw8Y8zbGPSoucyHCVuQuzEJSxuQTHl0cKauj7e2/ycgl1H/PVLsOetXCIGiho
9qGpSRUn6oP6a5O4pV/2wpYWLGBflT9nVy0NTxB+TGW9rXt3ga5pQeDqUzDJp0iD
v+2Nr5wGOx6HsB/97PFJoqmhgk5jl7qMyz/p/QIDAQABAoIBAQC09IBrqWD//urb
v/klVkHI82+xRrzMracWYgNDy8vWdXlPdmj1n0GySl2g8Sb8Btnp94UCt7shaBCs
1eGKOLpDglHeZmwX2Eo/RK1DdRAEYq/iwrIK7SI6uUVI10OzoPF02NhOUbyQHlyb
EqB0Ox8BzvEmNZTRLhWl15K6D3DzLr8s74htQ8jSXwrxZsn+TgNR3dDNHCAVfvEj
KTzxr1Wbh8Gj0DpkEDWDhO0LC3HQ1mH600iXl2AJmqhZY2x2a8+bRnRjN57qK8vw
j/Sf6OBrsj3RNw7ElourpKhVaPFlAMRPW/2g1LmmSk32wb/PVfptWouOqdVIfebU
XC1rMTXxAoGBAPML085gO/nQBM7r6tn6o5DOqklV2EWOgTWIhLL1qULOyMPaLDHA
BezGDGVwM1fh41aMPGMUCNi4tF6wrPgTm8+B3A+VeB/EuUdQKXwXAuVpT6bILbrv
tNo+xJosMxOxWfYGXTtgeulT7pBrt3UEIJO6N9v1aglHedGpLO08tHdrAoGBAN3m
/O94JOxXPfU4TWDBi3Kp5LONgjT7UphuKBKd9W0wMXmg2yCGBDd50WL5e0fOsZSn
F6W3FaRfuTPzlnpN0RBhbXUAto5TqrSHGcnHsq1AiCJR3M1DBJbidsB30B2MrGGk
4iDSeeOOwfpz/q6eADUMJ8B53wBcpp998CezOUY3AoGAOhkPrnuOmq7BF32JjEW/
p7PfPNyKcEYaWOOjZOwYY+HuV/LlMZdHU0wcVns1qsJSh7PKE50MaFHuu9vb3QlC
652NLyR5rI5oA8YDxKfzh0dfRO0BEXiPcMQkVv5Nz11LteXbj4AGoZPwqRE2r4An
EWkoRSorCMDPO/EGzkb+lLUCgYB5mttkNHgvmMPYfclRy98qsFYn29XOSSiDZmLJ
iQz1kGKcn/1V/nuTF0wbLFNWD7yblVJNO1893EN3Z/dAASMgEfRVGVXx+4HiOuN0
A980ONghzrDtP9vlAChJ458Ggo/5Vs2hZRbtEM/W+/hHI0lDyOyD+4WtAhlB8FdK
uloZ0wKBgE6GmrSgpTkbZJKvaxTS7NC7EqccR5tE/3BKNmbwNG8Ql+7whu1C+6ab
JAqCYtm+wlurVtz5ZDydXFFEnALIWs3x4gCLFD0YI1/2zf8LP6JF5C7eoUOMZ/g3
czOcv04ce+WlrtrwvWCysWlZPz3HVPgVQgXc9iy0uVNy/gbs/mMk
-----END RSA PRIVATE KEY-----
";

fn public_jwk() -> serde_json::Value {
    json!({
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KID, "e": "AQAB",
        "n": "0qyCnHMYukYoLXQicPZoz-iup1lKSfBWK5cedbDLmjW8GQeDxlYA2ARK7M2XwOd8z6xj4MUInAT1Fu6ezoNScSsCbDNYkDyxuQOtThFMEAUvhWfDqxTQy0fV88viudKVFQ6qxaKDF39pfs4VBgBE9z2pi0gaqYQBeR7MujYmUmkKuKKY-dnEw8Y8zbGPSoucyHCVuQuzEJSxuQTHl0cKauj7e2_ycgl1H_PVLsOetXCIGiho9qGpSRUn6oP6a5O4pV_2wpYWLGBflT9nVy0NTxB-TGW9rXt3ga5pQeDqUzDJp0iDv-2Nr5wGOx6HsB_97PFJoqmhgk5jl7qMyz_p_Q",
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

struct Harness {
    client: AnalysisCacheClient<Channel>,
    issuer: String,
    _issuer_server: MockServer,
    _container: ContainerAsync<Postgres>,
}

impl Harness {
    async fn start() -> Result<Self> {
        let container = Postgres::default().start().await.into_diagnostic()?;
        let host = container.get_host().await.into_diagnostic()?.to_string();
        let port = container.get_host_port_ipv4(5432).await.into_diagnostic()?;

        let endpoint = DbEndpoint::from_flags(
            &format!("{host}:{port}"),
            "postgres".to_string(),
            "postgres".to_string(),
            DbSslMode::Disable,
        )?;
        let pool = new_pool(
            endpoint,
            CredentialSource::Static("postgres".to_string()),
            4,
        )
        .await?;
        let store = Store::from_pool(pool);
        store.migrate().await?;

        let issuer_server = MockServer::start().await;
        let issuer = issuer_server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": issuer, "jwks_uri": format!("{issuer}/jwks"),
            })))
            .mount(&issuer_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "keys": [public_jwk()] })),
            )
            .mount(&issuer_server)
            .await;
        let verifier = Arc::new(
            OidcVerifier::new(&issuer, None)
                .await
                .map_err(|e| miette!("{e}"))?,
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .into_diagnostic()?;
        let address = format!("http://{}", listener.local_addr().into_diagnostic()?);
        let authenticated = ServiceBuilder::new()
            .layer(AuthenticationLayer::new(
                verifier,
                DEFAULT_TENANT_CLAIM.to_string(),
            ))
            .service(AnalysisCacheServer::new(Service::new(store)));
        tokio::spawn(
            Server::builder()
                .add_service(authenticated)
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );

        Ok(Self {
            client: AnalysisCacheClient::connect(address)
                .await
                .into_diagnostic()?,
            issuer,
            _issuer_server: issuer_server,
            _container: container,
        })
    }

    /// A request bearing a token this issuer signed, carrying `extra` on top of `iss`/`exp`.
    fn signed<T>(&self, extra: serde_json::Value, message: T) -> Result<Request<T>> {
        let mut claims = json!({ "iss": self.issuer, "sub": ACME, "exp": now() + 3600 });
        if let (Some(object), Some(extra)) = (claims.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(KID.to_string());
        let token = encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(PRIVATE_KEY_PEM.as_bytes()).into_diagnostic()?,
        )
        .into_diagnostic()?;

        let mut request = Request::new(message);
        let value: MetadataValue<_> = format!("Bearer {token}").parse().into_diagnostic()?;
        request.metadata_mut().insert("authorization", value);
        Ok(request)
    }

    /// A request carrying no credential at all.
    fn unsigned<T>(message: T) -> Request<T> {
        Request::new(message)
    }

    async fn put(&mut self, tenant: &str, key: &[u8], value: &[u8]) -> Result<i64> {
        let request = self.signed(
            json!({ DEFAULT_TENANT_CLAIM: tenant }),
            PutRequest {
                entries: vec![Entry {
                    key: key.to_vec(),
                    value: value.to_vec(),
                }],
            },
        )?;
        Ok(self
            .client
            .put(request)
            .await
            .into_diagnostic()?
            .into_inner()
            .stored)
    }

    async fn get(&mut self, tenant: &str, key: &[u8]) -> Result<Vec<Entry>> {
        let request = self.signed(
            json!({ DEFAULT_TENANT_CLAIM: tenant }),
            GetRequest {
                keys: vec![key.to_vec()],
            },
        )?;
        Ok(self
            .client
            .get(request)
            .await
            .into_diagnostic()?
            .into_inner()
            .entries)
    }

    /// The status a refused `Get` carries, or an error naming what was wrongly accepted.
    async fn refusal(&mut self, request: Request<GetRequest>) -> Result<Code> {
        Ok(self
            .client
            .get(request)
            .await
            .err()
            .ok_or_else(|| miette!("an unauthorized request was accepted"))?
            .code())
    }
}

#[tokio::test]
async fn an_entry_round_trips_through_the_rpcs_unchanged() -> Result<()> {
    let mut harness = Harness::start().await?;

    // Bazel's framing is opaque to the service; a leading zero byte is the case most likely to be
    // eaten by a parser that believes it understands the format.
    let value = vec![0x00, 0x02, b'h', b'i', 0xff];
    assert_eq!(harness.put(ACME, b"k", &value).await?, 1);

    let entries = harness.get(ACME, b"k").await?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].value, value);
    Ok(())
}

#[tokio::test]
async fn one_tenants_entries_are_invisible_to_another() -> Result<()> {
    let mut harness = Harness::start().await?;

    assert_eq!(harness.put(ACME, b"shared", b"mine").await?, 1);
    assert!(
        harness.get(BETA, b"shared").await?.is_empty(),
        "a second tenant read the first tenant's entry"
    );
    // The same key under a second tenant is a new row, not a conflict with the first.
    assert_eq!(harness.put(BETA, b"shared", b"theirs").await?, 1);
    assert_eq!(harness.get(ACME, b"shared").await?[0].value, b"mine");
    Ok(())
}

#[tokio::test]
async fn a_request_without_a_verifiable_tenant_never_reaches_storage() -> Result<()> {
    let mut harness = Harness::start().await?;
    let keys = || GetRequest {
        keys: vec![b"k".to_vec()],
    };

    assert_eq!(
        harness.refusal(Harness::unsigned(keys())).await?,
        Code::Unauthenticated
    );
    // Verified, but carrying nothing that names a tenant.
    let no_claim = harness.signed(json!({}), keys())?;
    assert_eq!(harness.refusal(no_claim).await?, Code::PermissionDenied);
    // Present and verified, but not a UUID: refused rather than read as absent.
    let malformed = harness.signed(json!({ DEFAULT_TENANT_CLAIM: "not-a-uuid" }), keys())?;
    assert_eq!(harness.refusal(malformed).await?, Code::PermissionDenied);
    // A claim of the wrong JSON type must not resolve either.
    let wrong_type = harness.signed(json!({ DEFAULT_TENANT_CLAIM: 7 }), keys())?;
    assert_eq!(harness.refusal(wrong_type).await?, Code::PermissionDenied);
    Ok(())
}

#[tokio::test]
async fn a_token_this_issuer_did_not_sign_is_refused() -> Result<()> {
    let mut harness = Harness::start().await?;

    // Right shape, right claims, wrong issuer: the layer must reject on the signature chain
    // rather than on anything it can read out of the payload.
    let mut request = Request::new(GetRequest {
        keys: vec![b"k".to_vec()],
    });
    let claims = json!({ "iss": "https://not.trusted.example", "sub": ACME,
                         "exp": now() + 3600, DEFAULT_TENANT_CLAIM: ACME });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let token = encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(PRIVATE_KEY_PEM.as_bytes()).into_diagnostic()?,
    )
    .into_diagnostic()?;
    let value: MetadataValue<_> = format!("Bearer {token}").parse().into_diagnostic()?;
    request.metadata_mut().insert("authorization", value);

    assert_eq!(harness.refusal(request).await?, Code::Unauthenticated);
    Ok(())
}

#[tokio::test]
async fn a_miss_omits_the_key_rather_than_returning_an_empty_value() -> Result<()> {
    let mut harness = Harness::start().await?;

    assert!(harness.get(ACME, b"never-written").await?.is_empty());
    Ok(())
}
