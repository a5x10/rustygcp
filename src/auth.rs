//! Where the access token comes from: nowhere (an emulator), a static token
//! (`GOOGLE_OAUTH_ACCESS_TOKEN`, e.g. `gcloud auth print-access-token
//! --impersonate-service-account=...` on a dev box, valid an hour), or the
//! metadata server (Cloud Run, a VM), cached until a minute before it expires.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{Error, Result, ok};

const METADATA_TOKEN: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Cheap to clone; clones share the HTTP client and the token cache.
#[derive(Clone)]
pub struct Auth(Arc<Inner>);

struct Inner {
    http: reqwest::Client,
    kind: Kind,
}

enum Kind {
    Emulator,
    Static(String),
    Metadata {
        url: String,
        cache: tokio::sync::Mutex<Option<(String, Instant)>>,
    },
}

/// Never the token.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0.kind {
            Kind::Emulator => "Auth(emulator)",
            Kind::Static(_) => "Auth(static token)",
            Kind::Metadata { .. } => "Auth(metadata server)",
        })
    }
}

impl Auth {
    /// `GOOGLE_OAUTH_ACCESS_TOKEN` when set and not empty, else the metadata server.
    pub fn from_env() -> Self {
        match std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN") {
            Ok(t) if !t.is_empty() => Self::static_token(t),
            _ => Self::metadata_at(METADATA_TOKEN),
        }
    }

    pub(crate) fn static_token(token: impl Into<String>) -> Self {
        Self::new(Kind::Static(token.into()))
    }

    /// For an emulator that checks no token (Firestore's takes `Bearer owner`).
    pub(crate) fn emulator() -> Self {
        Self::new(Kind::Emulator)
    }

    pub(crate) fn is_emulator(&self) -> bool {
        matches!(self.0.kind, Kind::Emulator)
    }

    pub(crate) fn metadata_at(url: &str) -> Self {
        Self::new(Kind::Metadata {
            url: url.into(),
            cache: tokio::sync::Mutex::new(None),
        })
    }

    fn new(kind: Kind) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("TLS backend"); // what reqwest::Client::new() panics on too
        Self(Arc::new(Inner { http, kind }))
    }

    /// The client every call goes through (10 s timeout unless a call sets its own).
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.0.http
    }

    /// The `Authorization` header value.
    pub(crate) async fn bearer(&self) -> Result<String> {
        match &self.0.kind {
            Kind::Emulator => Ok("Bearer owner".into()),
            Kind::Static(t) => Ok(format!("Bearer {t}")),
            Kind::Metadata { url, cache } => {
                // held across the fetch: concurrent callers wait for one token
                let mut cache = cache.lock().await;
                if let Some((t, until)) = cache.as_ref()
                    && Instant::now() < *until
                {
                    return Ok(format!("Bearer {t}"));
                }
                let resp = self
                    .0
                    .http
                    .get(url)
                    .header("Metadata-Flavor", "Google")
                    .send()
                    .await?;
                let v: Value = ok(resp).await?.json().await?;
                let token = v["access_token"]
                    .as_str()
                    .ok_or_else(|| Error::Http("metadata token missing".into()))?;
                let ttl = v["expires_in"].as_u64().unwrap_or(300).saturating_sub(60);
                *cache = Some((token.to_string(), Instant::now() + Duration::from_secs(ttl)));
                Ok(format!("Bearer {token}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fake;

    #[tokio::test]
    async fn the_metadata_token_is_fetched_once_and_refetched_when_it_expires() {
        // 61 s of life = 1 s after the minute of margin
        let server = fake(vec![
            (200, r#"{"access_token":"fake-1","expires_in":3599}"#.into()),
            (200, r#"{"access_token":"fake-2","expires_in":60}"#.into()),
            (200, r#"{"access_token":"fake-3","expires_in":3599}"#.into()),
        ])
        .await;

        let auth = Auth::metadata_at(&format!("{}/token", server.url));
        assert_eq!(auth.bearer().await.unwrap(), "Bearer fake-1");
        assert_eq!(auth.clone().bearer().await.unwrap(), "Bearer fake-1");
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            1,
            "the second call and the clone used the cache"
        );
        assert!(seen[0].starts_with("GET /token HTTP/1.1\r\n"));
        assert!(seen[0].contains("metadata-flavor: Google\r\n"));

        // a token inside its last minute is not served from the cache
        let auth = Auth::metadata_at(&format!("{}/token", server.url));
        assert_eq!(auth.bearer().await.unwrap(), "Bearer fake-2");
        assert_eq!(auth.bearer().await.unwrap(), "Bearer fake-3");
        assert_eq!(server.seen().len(), 3);
    }

    #[tokio::test]
    async fn a_refused_or_malformed_metadata_answer_is_an_error() {
        let server = fake(vec![(404, "{}".into()), (200, "{}".into())]).await;
        let auth = Auth::metadata_at(&format!("{}/token", server.url));
        assert!(matches!(
            auth.bearer().await,
            Err(Error::Status { status: 404, .. })
        ));
        assert!(matches!(auth.bearer().await, Err(Error::Http(_))));
    }

    #[test]
    fn debug_never_shows_the_token() {
        let auth = Auth::static_token("fake-token");
        assert_eq!(format!("{auth:?}"), "Auth(static token)");
        assert!(Auth::emulator().is_emulator() && !auth.is_emulator());
    }
}
