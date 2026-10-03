//! The check of a Google-signed OIDC token on an internal route: what Cloud
//! Scheduler and Cloud Tasks put in `Authorization` when a job or task is
//! configured with a service account. Fails closed: no token, no pass.

use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use serde::Deserialize;

const GOOGLE_CERTS: &str = "https://www.googleapis.com/oauth2/v3/certs";
const JWKS_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum OidcError {
    /// No token, or not one Google signed for this audience: a 401.
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    /// A valid Google token of somebody else: a 403.
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    /// Google's signing keys could not be fetched: a 500, the caller retries.
    #[error("google certs: {0}")]
    Keys(String),
}

#[derive(Deserialize)]
struct GoogleClaims {
    email: String,
    #[serde(default)]
    email_verified: bool,
}

/// Keeps Google's signing keys for an hour. One per process.
pub struct Verifier {
    http: reqwest::Client,
    certs_url: String,
    keys: tokio::sync::Mutex<Option<(JwkSet, Instant)>>,
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Verifier {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            certs_url: GOOGLE_CERTS.into(),
            keys: tokio::sync::Mutex::new(None),
        }
    }

    /// `authorization` is the request's `Authorization` header. Passes only a
    /// token signed by Google, not expired, with `aud` = `audience` and a
    /// verified `email` = `email` (the service account of the job or queue).
    pub async fn check(
        &self,
        authorization: Option<&str>,
        audience: &str,
        email: &str,
    ) -> Result<(), OidcError> {
        let token = authorization
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or(OidcError::Unauthorized("missing token"))?;
        let kid = jsonwebtoken::decode_header(token)
            .ok()
            .and_then(|h| h.kid)
            .ok_or(OidcError::Unauthorized("bad token"))?;
        let jwks = self.google_keys().await?;
        let jwk = jwks
            .find(&kid)
            .ok_or(OidcError::Unauthorized("unknown signing key"))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| OidcError::Unauthorized("bad key"))?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[audience]);
        v.set_issuer(&["https://accounts.google.com", "accounts.google.com"]);
        let claims = jsonwebtoken::decode::<GoogleClaims>(token, &key, &v)
            .map_err(|_| OidcError::Unauthorized("token rejected"))?
            .claims;
        if !claims.email_verified || claims.email != email {
            return Err(OidcError::Forbidden("not the expected service account"));
        }
        Ok(())
    }

    async fn google_keys(&self) -> Result<JwkSet, OidcError> {
        let mut cache = self.keys.lock().await;
        if let Some((set, at)) = cache.as_ref()
            && at.elapsed() < JWKS_TTL
        {
            return Ok(set.clone());
        }
        let keys = |e: reqwest::Error| OidcError::Keys(e.to_string());
        let set: JwkSet = self
            .http
            .get(&self.certs_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(keys)?
            .json()
            .await
            .map_err(keys)?;
        *cache = Some((set.clone(), Instant::now()));
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use jsonwebtoken::{EncodingKey, Header, jwk::Jwk};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use serde_json::{Value, json};

    use super::*;
    use crate::testing::{Fake, fake};

    const AUD: &str = "https://svc.example";
    const EMAIL: &str = "tasks@fake-project.iam.gserviceaccount.com";

    /// A throwaway signing key and the JWKS a certs endpoint would serve for it.
    fn key(kid: &str) -> (EncodingKey, String) {
        let rsa = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let key = EncodingKey::from_rsa_der(rsa.to_pkcs1_der().unwrap().as_bytes());
        let mut jwk = Jwk::from_encoding_key(&key, Algorithm::RS256).unwrap();
        jwk.common.key_id = Some(kid.into());
        (
            key,
            serde_json::to_string(&JwkSet { keys: vec![jwk] }).unwrap(),
        )
    }

    fn google() -> &'static (EncodingKey, String) {
        static KEY: OnceLock<(EncodingKey, String)> = OnceLock::new();
        KEY.get_or_init(|| key("fake-kid"))
    }

    fn sign(key: &EncodingKey, kid: &str, claims: Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        format!(
            "Bearer {}",
            jsonwebtoken::encode(&header, &claims, key).unwrap()
        )
    }

    fn claims() -> Value {
        json!({
            "iss": "https://accounts.google.com",
            "aud": AUD,
            "email": EMAIL,
            "email_verified": true,
            "exp": chrono::Utc::now().timestamp() + 600,
        })
    }

    /// A verifier whose certs endpoint answers `fetches` times.
    async fn verifier(fetches: usize) -> (Verifier, Fake) {
        let server = fake(vec![(200, google().1.clone()); fetches]).await;
        let mut v = Verifier::new();
        v.certs_url = server.url.clone();
        (v, server)
    }

    #[tokio::test]
    async fn a_google_token_for_this_audience_and_account_passes() {
        let (v, server) = verifier(1).await;
        let token = sign(&google().0, "fake-kid", claims());
        assert_eq!(v.check(Some(&token), AUD, EMAIL).await, Ok(()));
        assert_eq!(v.check(Some(&token), AUD, EMAIL).await, Ok(()));
        assert_eq!(server.seen().len(), 1, "the keys are cached");
    }

    #[tokio::test]
    async fn no_token_is_refused_before_any_key_fetch() {
        let (v, server) = verifier(0).await;
        for header in [None, Some(""), Some("Bearer not-a-jwt"), Some("Basic abc")] {
            assert!(
                matches!(
                    v.check(header, AUD, EMAIL).await,
                    Err(OidcError::Unauthorized(_))
                ),
                "{header:?}"
            );
        }
        // signed, but without a `kid` there is no key to look up
        let no_kid =
            jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims(), &google().0).unwrap();
        assert_eq!(
            v.check(Some(&format!("Bearer {no_kid}")), AUD, EMAIL).await,
            Err(OidcError::Unauthorized("bad token"))
        );
        assert!(server.seen().is_empty());
    }

    #[tokio::test]
    async fn every_wrong_claim_is_refused() {
        let (v, _server) = verifier(1).await;
        let with = |field: &str, value: Value| {
            let mut c = claims();
            c[field] = value;
            sign(&google().0, "fake-kid", c)
        };
        let rejected = Err(OidcError::Unauthorized("token rejected"));
        let other = Err(OidcError::Forbidden("not the expected service account"));
        let now = chrono::Utc::now().timestamp();

        let cases = [
            (
                "audience",
                with("aud", json!("https://other.example")),
                &rejected,
            ),
            (
                "issuer",
                with("iss", json!("https://evil.example")),
                &rejected,
            ),
            ("expired", with("exp", json!(now - 600)), &rejected),
            (
                "email",
                with(
                    "email",
                    json!("someone@fake-project.iam.gserviceaccount.com"),
                ),
                &other,
            ),
            ("unverified", with("email_verified", json!(false)), &other),
        ];
        for (what, token, want) in &cases {
            assert_eq!(&v.check(Some(token), AUD, EMAIL).await, *want, "{what}");
        }
        // the caller's expectations are part of the check too
        let good = sign(&google().0, "fake-kid", claims());
        assert_eq!(
            v.check(Some(&good), "https://other.example", EMAIL).await,
            rejected
        );
        assert_eq!(v.check(Some(&good), AUD, "other@example.com").await, other);
    }

    #[tokio::test]
    async fn a_token_signed_by_another_key_is_refused() {
        let (v, _server) = verifier(1).await;
        let (stranger, _) = key("fake-kid");
        // same kid as Google's key, another private key: the signature fails
        assert_eq!(
            v.check(Some(&sign(&stranger, "fake-kid", claims())), AUD, EMAIL)
                .await,
            Err(OidcError::Unauthorized("token rejected"))
        );
        assert_eq!(
            v.check(Some(&sign(&stranger, "other-kid", claims())), AUD, EMAIL)
                .await,
            Err(OidcError::Unauthorized("unknown signing key"))
        );
    }

    #[tokio::test]
    async fn unreachable_keys_are_an_error_not_a_pass() {
        let server = fake(vec![(500, "{}".into())]).await;
        let mut v = Verifier::new();
        v.certs_url = server.url.clone();
        let token = sign(&google().0, "fake-kid", claims());
        assert!(matches!(
            v.check(Some(&token), AUD, EMAIL).await,
            Err(OidcError::Keys(_))
        ));
    }
}
