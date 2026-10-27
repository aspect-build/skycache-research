//! Bearer-token verification against trusted OIDC issuers.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Verified claims. Only `sub` is required; the rest are kept so a deployment can name its own
/// tenant claim.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub enum TenantClaim<'a> {
    /// Absent, JSON `null`, or blank.
    Absent,
    /// Present but not a JSON string. Refused rather than read as absent.
    NotString,
    /// Trimmed and non-empty.
    Value(&'a str),
}

impl Claims {
    #[must_use]
    pub fn tenant_claim(&self, claim: &str) -> TenantClaim<'_> {
        match self.extra.get(claim) {
            None | Some(serde_json::Value::Null) => TenantClaim::Absent,
            Some(value) => match value.as_str() {
                None => TenantClaim::NotString,
                Some(tenant) => match tenant.trim() {
                    "" => TenantClaim::Absent,
                    tenant => TenantClaim::Value(tenant),
                },
            },
        }
    }
}

#[derive(Debug)]
pub enum OidcError {
    Malformed,
    UnknownKid,
    Invalid(String),
    Fetch(String),
}

/// The token from an `Authorization: Bearer <token>` value. The scheme is case-insensitive per
/// RFC 7235.
#[must_use]
pub fn parse_bearer(header: &str) -> Option<&str> {
    header
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token)
}

/// Parses `<issuer>=<audience>`, splitting on the first `=`: an issuer URL never contains one.
pub fn parse_issuer_audience(s: &str) -> std::result::Result<(String, String), String> {
    match s.split_once('=') {
        Some((issuer, audience)) if !issuer.is_empty() && !audience.is_empty() => {
            Ok((issuer.to_string(), audience.to_string()))
        }
        _ => Err(format!(
            "expected <issuer>=<audience> with both non-empty, got {s:?}"
        )),
    }
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: String,
}

type Keys = HashMap<String, DecodingKey>;

/// Floors JWKS refreshes so a flood of unknown key ids cannot hammer the issuer.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

/// Re-fetches the JWKS at this age so a key the issuer revokes stops verifying within the bound.
const JWKS_TTL: Duration = Duration::from_secs(3600);

/// A hung issuer would otherwise stall startup until some upstream gateway gives up.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Startup retries ride out a blip; a longer outage exits and leaves backoff to the orchestrator.
const STARTUP_ATTEMPTS: u32 = 3;
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(1);

struct IssuerVerifier {
    issuer: String,
    audiences: Vec<String>,
    http: reqwest::Client,
    jwks_uri: String,
    keys: RwLock<Arc<Keys>>,
    refresh: Mutex<RefreshState>,
}

struct RefreshState {
    last_attempt: Instant,
    failed: bool,
}

impl IssuerVerifier {
    async fn new(issuer: &str, audiences: &[String]) -> Result<Self> {
        let issuer = issuer.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .into_diagnostic()
            .wrap_err("building the OIDC HTTP client")?;
        let mut attempt: u32 = 1;
        let (jwks_uri, keys) = loop {
            match discover_and_fetch(&http, &issuer).await {
                Ok(pair) => break pair,
                Err(err) if attempt < STARTUP_ATTEMPTS => {
                    tracing::warn!(
                        issuer = %issuer,
                        attempt,
                        error = format!("{err:#}"),
                        "OIDC discovery failed at startup; retrying"
                    );
                    attempt += 1;
                    tokio::time::sleep(STARTUP_RETRY_DELAY).await;
                }
                Err(err) => return Err(err),
            }
        };
        Ok(Self {
            issuer,
            audiences: audiences.to_vec(),
            http,
            jwks_uri,
            keys: RwLock::new(Arc::new(keys)),
            refresh: Mutex::new(RefreshState {
                last_attempt: Instant::now(),
                failed: false,
            }),
        })
    }

    fn validation_for(&self, alg: Algorithm) -> Validation {
        let mut validation = Validation::new(alg);
        validation.set_issuer(&[self.issuer.as_str()]);
        if self.audiences.is_empty() {
            validation.validate_aud = false;
        } else {
            validation.required_spec_claims.insert("aud".to_string());
            validation.set_audience(&self.audiences);
        }
        validation
    }

    async fn verify(&self, token: &str) -> std::result::Result<Claims, OidcError> {
        let header = decode_header(token).map_err(|_| OidcError::Malformed)?;
        // Rejects `none`, HMAC and algorithm confusion before any key is selected.
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(OidcError::Invalid(format!(
                "unsupported algorithm {:?}",
                header.alg
            )));
        }
        let kid = header.kid.ok_or(OidcError::Malformed)?;
        let validation = self.validation_for(header.alg);
        // A failed proactive refresh is ignored: a cached key still verifies through an outage.
        if self.refresh.lock().await.last_attempt.elapsed() >= JWKS_TTL {
            let _ = self.refresh_keys().await;
        }
        if let Some(claims) = self.decode_with_cached_key(token, &kid, &validation)? {
            return Ok(claims);
        }
        self.refresh_keys().await?;
        self.decode_with_cached_key(token, &kid, &validation)?
            .ok_or(OidcError::UnknownKid)
    }

    /// `Ok(None)` means the kid is not cached; `Err` means a known key rejected the token.
    fn decode_with_cached_key(
        &self,
        token: &str,
        kid: &str,
        validation: &Validation,
    ) -> std::result::Result<Option<Claims>, OidcError> {
        let keys = self
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(key) = keys.get(kid) else {
            return Ok(None);
        };
        let data = decode::<Claims>(token, key, validation)
            .map_err(|e| OidcError::Invalid(e.to_string()))?;
        Ok(Some(data.claims))
    }

    /// A failed attempt stays failed through the cooldown rather than reading as `UnknownKid`.
    async fn refresh_keys(&self) -> std::result::Result<(), OidcError> {
        let mut refresh = self.refresh.lock().await;
        if refresh.last_attempt.elapsed() < REFRESH_COOLDOWN {
            return if refresh.failed {
                Err(OidcError::Fetch(
                    "the previous key refresh failed".to_string(),
                ))
            } else {
                Ok(())
            };
        }
        refresh.last_attempt = Instant::now();
        refresh.failed = true;
        let keys = fetch_keys(&self.http, &self.jwks_uri)
            .await
            .map_err(|e| OidcError::Fetch(format!("{e}")))?;
        *self.keys.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(keys);
        refresh.failed = false;
        Ok(())
    }
}

/// Routes each token to the trusted issuer its `iss` names.
pub struct OidcVerifier {
    issuers: Vec<IssuerVerifier>,
}

impl OidcVerifier {
    pub async fn new(issuer: &str, audience: Option<&str>) -> Result<Self> {
        let audiences: Vec<String> = audience.into_iter().map(str::to_owned).collect();
        Self::new_per_issuer([(issuer, audiences.as_slice())]).await
    }

    /// Each issuer checks `aud` against its own any-of list; an empty list disables the check.
    pub async fn new_per_issuer<'a, I>(specs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (&'a str, &'a [String])>,
    {
        let mut built: Vec<IssuerVerifier> = Vec::new();
        for (issuer, audiences) in specs {
            let verifier = IssuerVerifier::new(issuer, audiences).await?;
            if built
                .iter()
                .any(|existing| existing.issuer == verifier.issuer)
            {
                return Err(miette!("duplicate OIDC issuer {}", verifier.issuer));
            }
            built.push(verifier);
        }
        if built.is_empty() {
            return Err(miette!("at least one OIDC issuer is required"));
        }
        Ok(Self { issuers: built })
    }

    /// `issuer_audiences` pins per issuer; `global_audience` applies to any issuer without one.
    /// A pin naming an untrusted issuer is a configuration error, not something to ignore.
    pub async fn from_cli(
        issuers: &[String],
        issuer_audiences: &[(String, String)],
        global_audience: Option<&str>,
    ) -> Result<Self> {
        if issuers.is_empty() {
            return Err(miette!("at least one OIDC issuer is required"));
        }
        let mut by_issuer: HashMap<&str, Vec<String>> = HashMap::new();
        for (issuer, aud) in issuer_audiences {
            by_issuer.entry(issuer).or_default().push(aud.clone());
        }
        if let Some(orphan) = by_issuer
            .keys()
            .find(|issuer| !issuers.iter().any(|i| i == *issuer))
        {
            return Err(miette!(
                "--oidc-issuer-audience names issuer {orphan}, which is not a trusted --oidc-issuer"
            ));
        }
        let global: Vec<String> = global_audience.into_iter().map(str::to_owned).collect();
        let specs: Vec<(&str, Vec<String>)> = issuers
            .iter()
            .map(|issuer| {
                let auds = by_issuer
                    .get(issuer.as_str())
                    .cloned()
                    .unwrap_or_else(|| global.clone());
                (issuer.as_str(), auds)
            })
            .collect();
        Self::new_per_issuer(specs.iter().map(|(iss, auds)| (*iss, auds.as_slice()))).await
    }

    /// The unverified `iss` only selects a keyset; that issuer's verifier re-checks `iss` and the
    /// signature, so a token naming a trusted issuer but signed by another key is refused.
    pub async fn verify(&self, token: &str) -> std::result::Result<Claims, OidcError> {
        let iss = unverified_issuer(token).ok_or(OidcError::Malformed)?;
        let verifier = self
            .issuers
            .iter()
            .find(|verifier| verifier.issuer == iss)
            .ok_or_else(|| OidcError::Invalid(format!("untrusted issuer {iss}")))?;
        verifier.verify(token).await
    }
}

fn unverified_issuer(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let iss = value.get("iss")?.as_str()?;
    Some(iss.trim_end_matches('/').to_string())
}

async fn discover_and_fetch(http: &reqwest::Client, issuer: &str) -> Result<(String, Keys)> {
    let url = format!("{issuer}/.well-known/openid-configuration");
    let discovery: Discovery = http
        .get(&url)
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("fetching OIDC discovery from {url}"))?
        .error_for_status()
        .into_diagnostic()
        .wrap_err("OIDC discovery returned an error status")?
        .json()
        .await
        .into_diagnostic()
        .wrap_err("parsing the OIDC discovery document")?;
    let keys = fetch_keys(http, &discovery.jwks_uri).await?;
    Ok((discovery.jwks_uri, keys))
}

async fn fetch_keys(http: &reqwest::Client, jwks_uri: &str) -> Result<Keys> {
    let jwks: JwkSet = http
        .get(jwks_uri)
        .send()
        .await
        .into_diagnostic()
        .wrap_err("fetching the issuer JWKS")?
        .error_for_status()
        .into_diagnostic()
        .wrap_err("the issuer JWKS returned an error status")?
        .json()
        .await
        .into_diagnostic()
        .wrap_err("parsing the issuer JWKS")?;
    let mut keys = Keys::new();
    for jwk in &jwks.keys {
        let Some(kid) = jwk.common.key_id.clone() else {
            continue;
        };
        let key = DecodingKey::from_jwk(jwk)
            .into_diagnostic()
            .wrap_err_with(|| format!("building a decoding key for kid {kid}"))?;
        keys.insert(kid, key);
    }
    Ok(keys)
}
