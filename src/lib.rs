//! The few Google Cloud REST calls a small Cloud Run service needs, over
//! reqwest + rustls: Firestore documents, Cloud Storage objects, Cloud Tasks
//! HTTP tasks, and the check of a Google-signed OIDC token. No service logic.
//!
//! One [`Auth`] is shared by the clients, so a process fetches and caches one
//! access token.

mod auth;
pub mod firestore;
pub mod gcs;
pub mod oidc;
pub mod tasks;
#[cfg(test)]
mod testing;

pub use auth::Auth;
pub use firestore::{Doc, Firestore, OpCounts, Precondition, Write};
pub use gcs::{Gcs, Object};
pub use oidc::{OidcError, Verifier};
pub use tasks::{HttpTask, Tasks};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A Firestore commit lost its precondition: the document changed, already
    /// exists, or is missing. Nothing of the commit was written.
    #[error("precondition failed")]
    Precondition,
    /// Google answered with an error. `code` is its `error.status`
    /// (`NOT_FOUND`, `PERMISSION_DENIED`, ...), empty when the body had none.
    #[error("{status} {code}: {message}")]
    Status {
        status: u16,
        code: String,
        message: String,
    },
    /// No usable answer: connect, timeout, or a body that is not what the API
    /// documents.
    #[error("http: {0}")]
    Http(String),
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

/// The response if it is a success, else Google's error body as [`Error::Status`].
pub(crate) async fn ok(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let v: serde_json::Value = resp.json().await.unwrap_or_default();
    Err(Error::Status {
        status,
        code: v["error"]["status"].as_str().unwrap_or_default().into(),
        message: v["error"]["message"]
            .as_str()
            .unwrap_or("no message")
            .into(),
    })
}
