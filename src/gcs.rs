//! Cloud Storage objects of one bucket over the JSON API: get, put, delete, list.

use std::time::Duration;

use reqwest::Url;
use serde_json::Value;

use crate::{Auth, Error, Result, ok};

/// Whole objects travel in one request, so the 10 s default is too short.
const TIMEOUT: Duration = Duration::from_secs(60);

pub struct Gcs {
    auth: Auth,
    bucket: String,
    root: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

impl Gcs {
    pub fn new(auth: &Auth, bucket: &str) -> Self {
        Self {
            auth: auth.clone(),
            bucket: bucket.into(),
            root: "https://storage.googleapis.com".into(),
        }
    }

    /// `<root>/<prefix>/b/<bucket>/o[/<object>]`, the object name as ONE path
    /// segment (its `/` become `%2F`).
    fn url(&self, prefix: &[&str], object: Option<&str>) -> Url {
        let mut url = Url::parse(&self.root).expect("root is a URL");
        url.path_segments_mut()
            .expect("root is http(s)")
            .extend(prefix)
            .extend(["b", &self.bucket, "o"])
            .extend(object);
        url
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let req = req.header("authorization", self.auth.bearer().await?);
        Ok(req.timeout(TIMEOUT).send().await?)
    }

    /// The object, or `None` when there is none.
    pub async fn get(&self, name: &str) -> Result<Option<Object>> {
        let url = self.url(&["storage", "v1"], Some(name));
        let resp = self
            .send(self.auth.http().get(url).query(&[("alt", "media")]))
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = ok(resp).await?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        Ok(Some(Object {
            bytes: resp.bytes().await?.to_vec(),
            content_type,
        }))
    }

    /// Create or replace the object.
    pub async fn put(&self, name: &str, content_type: &str, body: Vec<u8>) -> Result<()> {
        let url = self.url(&["upload", "storage", "v1"], None);
        let req = self
            .auth
            .http()
            .post(url)
            .query(&[("uploadType", "media"), ("name", name)])
            .header("content-type", content_type)
            .body(body);
        ok(self.send(req).await?).await?;
        Ok(())
    }

    /// `false` when there was nothing to delete.
    pub async fn delete(&self, name: &str) -> Result<bool> {
        let url = self.url(&["storage", "v1"], Some(name));
        let resp = self.send(self.auth.http().delete(url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        ok(resp).await?;
        Ok(true)
    }

    /// `(name, size in bytes)` of every object whose name starts with `prefix`.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let url = self.url(&["storage", "v1"], None);
        let mut out = Vec::new();
        let mut page = String::new();
        loop {
            let query = [
                ("prefix", prefix),
                ("fields", "items(name,size),nextPageToken"),
                ("pageToken", page.as_str()),
            ];
            let resp = self
                .send(self.auth.http().get(url.clone()).query(&query))
                .await?;
            let v: Value = ok(resp).await?.json().await?;
            for item in v["items"].as_array().into_iter().flatten() {
                // the JSON API sends `size` as a string
                let (Some(name), Some(size)) = (
                    item["name"].as_str(),
                    item["size"].as_str().and_then(|s| s.parse().ok()),
                ) else {
                    return Err(Error::Http(format!("gcs list: unexpected item {item}")));
                };
                out.push((name.to_string(), size));
            }
            match v["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() => page = t.to_string(),
                _ => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Fake, fake};

    async fn gcs(replies: Vec<(u16, &str)>) -> (Gcs, Fake) {
        let server = fake(
            replies
                .into_iter()
                .map(|(s, b)| (s, b.to_string()))
                .collect(),
        )
        .await;
        let mut gcs = Gcs::new(&Auth::static_token("fake-token"), "fake-bucket");
        gcs.root = server.url.clone();
        (gcs, server)
    }

    #[tokio::test]
    async fn get_asks_for_the_media_of_one_encoded_segment() {
        let (gcs, server) = gcs(vec![(200, r#"{"k":1}"#), (404, "{}"), (403, "{}")]).await;
        assert_eq!(
            gcs.get("images/a b/1.png").await.unwrap(),
            Some(Object {
                bytes: br#"{"k":1}"#.to_vec(),
                content_type: "application/json".into(),
            })
        );
        assert_eq!(gcs.get("absent").await.unwrap(), None);
        assert!(matches!(
            gcs.get("denied").await,
            Err(Error::Status { status: 403, .. })
        ));
        let seen = server.seen();
        assert!(
            seen[0].starts_with(
                "GET /storage/v1/b/fake-bucket/o/images%2Fa%20b%2F1.png?alt=media HTTP/1.1\r\n"
            ),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains("authorization: Bearer fake-token\r\n"));
    }

    #[tokio::test]
    async fn put_uploads_the_bytes_with_their_type() {
        let (gcs, server) = gcs(vec![(200, "{}"), (403, r#"{"error":{"message":"no"}}"#)]).await;
        gcs.put("images/1.png", "image/png", b"\x89PNG".to_vec())
            .await
            .unwrap();
        let denied = gcs.put("x", "text/plain", vec![]).await.unwrap_err();
        assert_eq!(denied.to_string(), "403 : no");
        let seen = server.seen();
        assert!(
            seen[0].starts_with(
                "POST /upload/storage/v1/b/fake-bucket/o?uploadType=media&name=images%2F1.png HTTP/1.1\r\n"
            ),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains("content-type: image/png\r\n"));
        assert!(seen[0].contains("content-length: 4\r\n"));
        assert!(seen[0].contains("authorization: Bearer fake-token\r\n"));
    }

    #[tokio::test]
    async fn delete_says_whether_there_was_an_object() {
        let (gcs, server) = gcs(vec![(204, ""), (404, "{}")]).await;
        assert!(gcs.delete("a/b").await.unwrap());
        assert!(!gcs.delete("a/b").await.unwrap());
        assert!(
            server.seen()[0].starts_with("DELETE /storage/v1/b/fake-bucket/o/a%2Fb HTTP/1.1\r\n")
        );
    }

    #[tokio::test]
    async fn list_follows_the_pages() {
        let (gcs, server) = gcs(vec![
            (
                200,
                r#"{"items":[{"name":"p/1","size":"10"}],"nextPageToken":"t2"}"#,
            ),
            (200, r#"{"items":[{"name":"p/2","size":"0"}]}"#),
            (200, "{}"),
        ])
        .await;
        assert_eq!(
            gcs.list("p/").await.unwrap(),
            [("p/1".to_string(), 10), ("p/2".to_string(), 0)]
        );
        assert_eq!(gcs.list("none/").await.unwrap(), []);
        let seen = server.seen();
        assert!(
            seen[0].starts_with(
                "GET /storage/v1/b/fake-bucket/o?prefix=p%2F&fields=items%28name%2Csize%29%2CnextPageToken&pageToken= HTTP/1.1\r\n"
            ),
            "{}",
            seen[0]
        );
        assert!(seen[1].contains("&pageToken=t2 HTTP/1.1\r\n"));
    }
}
