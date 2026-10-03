//! A Firestore client over its REST API: get, list, query, commit with
//! preconditions and `increment` transforms, delete. Documents are raw typed
//! fields (`{"stringValue": ...}`); the service owns the mapping.
//!
//! The endpoint is the emulator when `FIRESTORE_EMULATOR_HOST` is set, else
//! production with the given [`Auth`]. The database is `(default)` unless
//! `FIRESTORE_DATABASE` says otherwise.
// rustytail: one client type, extracted whole from the service it came from
// (475 lines there); a split would scatter the methods of one struct over files.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use serde_json::{Map, Value, json};

use crate::{Auth, Error, Result, ok};

/// Firestore operations this process has billed, counted the way Firestore
/// bills them (a query or a list costs at least one read), to set against
/// Cloud Monitoring.
#[derive(Debug, Default)]
pub struct OpCounts {
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub deletes: AtomicU64,
}

impl OpCounts {
    /// `(reads, writes, deletes)` since start.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.reads.load(Relaxed),
            self.writes.load(Relaxed),
            self.deletes.load(Relaxed),
        )
    }
}

pub struct Firestore {
    pub ops: OpCounts,
    auth: Auth,
    /// `projects/<p>/databases/(default)/documents`
    db_path: String,
    /// `https://firestore.googleapis.com/v1` or `http://<emulator>/v1`
    root: String,
}

/// One document as Firestore returns it: raw typed fields plus the
/// `update_time` a precondition needs.
#[derive(Debug, Clone)]
pub struct Doc {
    pub fields: Map<String, Value>,
    pub update_time: String,
}

impl Doc {
    pub fn string(&self, field: &str) -> Option<&str> {
        self.fields.get(field)?.get("stringValue")?.as_str()
    }

    pub fn integer(&self, field: &str) -> Option<i64> {
        self.fields
            .get(field)?
            .get("integerValue")?
            .as_str()?
            .parse()
            .ok()
    }

    /// Every integer field, keyed by field name (counters documents).
    pub fn integers(&self) -> impl Iterator<Item = (&str, i64)> {
        self.fields
            .keys()
            .filter_map(|k| Some((k.as_str(), self.integer(k)?)))
    }
}

#[derive(Debug, Clone)]
pub enum Precondition {
    None,
    /// `false` = create-if-absent; `true` = must already exist.
    Exists(bool),
    /// The document's `update_time` must still be this.
    UpdateTime(String),
}

#[derive(Debug, Clone)]
pub enum Write {
    /// Replace the whole document with `fields`.
    Put {
        path: String,
        fields: Map<String, Value>,
        pre: Precondition,
    },
    /// Set these top-level fields and leave the others, creating the document
    /// as needed (one entry of a shard document).
    Merge {
        path: String,
        fields: Map<String, Value>,
        pre: Precondition,
    },
    /// Add to integer fields, creating the document and fields as needed.
    Increment {
        path: String,
        by: Vec<(String, i64)>,
    },
    Delete {
        path: String,
    },
}

impl Firestore {
    /// The emulator when `FIRESTORE_EMULATOR_HOST` is set, else production.
    pub fn from_env(project: &str, auth: &Auth) -> Self {
        match std::env::var("FIRESTORE_EMULATOR_HOST") {
            Ok(host) => Self::emulator(&host, project),
            Err(_) => Self::new(project, auth),
        }
    }

    fn new(project: &str, auth: &Auth) -> Self {
        Self::at(
            "https://firestore.googleapis.com/v1".into(),
            project,
            auth.clone(),
        )
    }

    fn emulator(host: &str, project: &str) -> Self {
        Self::at(format!("http://{host}/v1"), project, Auth::emulator())
    }

    fn at(root: String, project: &str, auth: Auth) -> Self {
        let db = std::env::var("FIRESTORE_DATABASE").unwrap_or_else(|_| "(default)".into());
        Self {
            ops: OpCounts::default(),
            auth,
            db_path: format!("projects/{project}/databases/{db}/documents"),
            root,
        }
    }

    /// True when talking to the emulator, which has no other Google service beside it.
    pub fn is_emulator(&self) -> bool {
        self.auth.is_emulator()
    }

    fn name(&self, path: &str) -> String {
        format!("{}/{path}", self.db_path)
    }

    /// The full resource name of a document, for `referenceValue` filters.
    pub fn reference(&self, path: &str) -> Value {
        json!({ "referenceValue": self.name(path) })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        Ok(req
            .header("authorization", self.auth.bearer().await?)
            .send()
            .await?)
    }

    fn post(&self, method: &str, body: Value) -> reqwest::RequestBuilder {
        let url = format!("{}/{}:{method}", self.root, self.db_path);
        self.auth.http().post(url).json(&body)
    }

    pub async fn get(&self, path: &str) -> Result<Option<Doc>> {
        self.ops.reads.fetch_add(1, Relaxed);
        let url = format!("{}/{}", self.root, self.name(path));
        let resp = self.send(self.auth.http().get(url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(to_doc(&ok(resp).await?.json().await?)))
    }

    /// Every document of a collection, `(id, doc)`.
    pub async fn list(&self, collection: &str) -> Result<Vec<(String, Doc)>> {
        let url = format!("{}/{}", self.root, self.name(collection));
        let mut out = Vec::new();
        let mut page = String::new();
        loop {
            let query = [("pageSize", "300"), ("pageToken", page.as_str())];
            let resp = self.send(self.auth.http().get(&url).query(&query)).await?;
            let v: Value = ok(resp).await?.json().await?;
            let before = out.len();
            out.extend(
                v["documents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(with_id),
            );
            self.ops
                .reads
                .fetch_add((out.len() - before).max(1) as u64, Relaxed);
            match v["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() => page = t.to_string(),
                _ => return Ok(out),
            }
        }
    }

    /// A structured query over one top-level collection, `(id, doc)` in the
    /// order Firestore returns them. `filter` is a raw `where` clause;
    /// `by_id_asc` orders by document id. A single-field filter needs no
    /// composite index.
    pub async fn query(
        &self,
        collection: &str,
        filter: Option<Value>,
        by_id_asc: bool,
        limit: Option<u32>,
    ) -> Result<Vec<(String, Doc)>> {
        let mut q = json!({ "from": [{ "collectionId": collection }] });
        if let Some(f) = filter {
            q["where"] = f;
        }
        if by_id_asc {
            q["orderBy"] =
                json!([{ "field": { "fieldPath": "__name__" }, "direction": "ASCENDING" }]);
        }
        if let Some(n) = limit {
            q["limit"] = json!(n);
        }
        let resp = self
            .send(self.post("runQuery", json!({ "structuredQuery": q })))
            .await?;
        let v: Value = ok(resp).await?.json().await?;
        let docs: Vec<(String, Doc)> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| with_id(r.get("document")?))
            .collect();
        self.ops.reads.fetch_add(docs.len().max(1) as u64, Relaxed);
        Ok(docs)
    }

    /// The top-level collections (an export walks them).
    pub async fn collection_ids(&self) -> Result<Vec<String>> {
        self.ops.reads.fetch_add(1, Relaxed);
        let resp = self
            .send(self.post("listCollectionIds", json!({ "pageSize": 300 })))
            .await?;
        let v: Value = ok(resp).await?.json().await?;
        Ok(v["collectionIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect())
    }

    /// Commit writes atomically (at most 500). Returns each write's
    /// `update_time`. [`Error::Precondition`] when any precondition failed:
    /// nothing was written.
    pub async fn commit(&self, writes: &[Write]) -> Result<Vec<String>> {
        for w in writes {
            match w {
                Write::Delete { .. } => self.ops.deletes.fetch_add(1, Relaxed),
                _ => self.ops.writes.fetch_add(1, Relaxed),
            };
        }
        let writes: Vec<Value> = writes.iter().map(|w| self.write_json(w)).collect();
        let resp = self
            .send(self.post("commit", json!({ "writes": writes })))
            .await?;
        let v: Value = match ok(resp).await {
            Ok(resp) => resp.json().await?,
            // NOT_FOUND is how `Exists(true)` fails on a missing document
            Err(Error::Status { code, .. })
                if matches!(
                    code.as_str(),
                    "FAILED_PRECONDITION" | "ALREADY_EXISTS" | "ABORTED" | "NOT_FOUND"
                ) =>
            {
                return Err(Error::Precondition);
            }
            Err(e) => return Err(e),
        };
        Ok(v["writeResults"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r["updateTime"].as_str().unwrap_or_default().to_string())
            .collect())
    }

    fn write_json(&self, w: &Write) -> Value {
        let (mut v, pre) = match w {
            Write::Put { path, fields, pre } => (
                json!({ "update": { "name": self.name(path), "fields": fields } }),
                pre,
            ),
            Write::Merge { path, fields, pre } => (
                json!({
                    "update": { "name": self.name(path), "fields": fields },
                    "updateMask": {
                        "fieldPaths": fields.keys().map(|f| quote_field(f)).collect::<Vec<_>>(),
                    },
                }),
                pre,
            ),
            Write::Increment { path, by } => {
                return json!({
                    "update": { "name": self.name(path), "fields": {} },
                    "updateMask": { "fieldPaths": [] },
                    "updateTransforms": by.iter().map(|(f, n)| json!({
                        "fieldPath": quote_field(f),
                        "increment": { "integerValue": n.to_string() },
                    })).collect::<Vec<_>>(),
                });
            }
            Write::Delete { path } => return json!({ "delete": self.name(path) }),
        };
        match pre {
            Precondition::None => {}
            Precondition::Exists(e) => v["currentDocument"] = json!({ "exists": e }),
            Precondition::UpdateTime(t) => v["currentDocument"] = json!({ "updateTime": t }),
        }
        v
    }
}

/// A string field value.
pub fn string_value(s: impl Into<String>) -> Value {
    json!({ "stringValue": s.into() })
}

pub fn integer_value(n: i64) -> Value {
    json!({ "integerValue": n.to_string() })
}

pub fn timestamp_value(t: chrono::DateTime<chrono::Utc>) -> Value {
    json!({ "timestampValue": t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true) })
}

/// Field names like `<uuid>|<uuid>` are not plain identifiers, so a field
/// path quotes them in backticks.
fn quote_field(f: &str) -> String {
    format!("`{}`", f.replace('\\', "\\\\").replace('`', "\\`"))
}

fn to_doc(v: &Value) -> Doc {
    Doc {
        fields: v["fields"].as_object().cloned().unwrap_or_default(),
        update_time: v["updateTime"].as_str().unwrap_or_default().to_string(),
    }
}

/// `(id, doc)` of a document resource.
fn with_id(d: &Value) -> Option<(String, Doc)> {
    let id = d["name"].as_str()?.rsplit('/').next()?.to_string();
    Some((id, to_doc(d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn writes_are_the_json_firestore_documents() {
        let fs = Firestore::new("fake-project", &Auth::static_token("fake-token"));
        let name = "projects/fake-project/databases/(default)/documents/games/g1";
        let put = |pre| Write::Put {
            path: "games/g1".into(),
            fields: fields(json!({ "phase": integer_value(2) })),
            pre,
        };
        let update = json!({ "name": name, "fields": { "phase": { "integerValue": "2" } } });

        assert_eq!(
            fs.write_json(&put(Precondition::None)),
            json!({ "update": update })
        );
        assert_eq!(
            fs.write_json(&put(Precondition::Exists(false))),
            json!({ "update": update, "currentDocument": { "exists": false } })
        );
        assert_eq!(
            fs.write_json(&put(Precondition::UpdateTime(
                "2026-10-03T00:00:00.000001Z".into()
            ))),
            json!({
                "update": update,
                "currentDocument": { "updateTime": "2026-10-03T00:00:00.000001Z" },
            })
        );
        assert_eq!(
            fs.write_json(&Write::Merge {
                path: "games/g1".into(),
                fields: fields(json!({ "a`b": string_value("x") })),
                pre: Precondition::Exists(true),
            }),
            json!({
                "update": { "name": name, "fields": { "a`b": { "stringValue": "x" } } },
                "updateMask": { "fieldPaths": ["`a\\`b`"] },
                "currentDocument": { "exists": true },
            })
        );
        assert_eq!(
            fs.write_json(&Write::Increment {
                path: "games/g1".into(),
                by: vec![("u1|u2".into(), -3)],
            }),
            json!({
                "update": { "name": name, "fields": {} },
                "updateMask": { "fieldPaths": [] },
                "updateTransforms": [
                    { "fieldPath": "`u1|u2`", "increment": { "integerValue": "-3" } },
                ],
            })
        );
        assert_eq!(
            fs.write_json(&Write::Delete {
                path: "games/g1".into()
            }),
            json!({ "delete": name })
        );
        assert_eq!(fs.reference("games/g1"), json!({ "referenceValue": name }));
    }

    #[test]
    fn values_and_accessors() {
        let t = chrono::DateTime::from_timestamp(1_790_000_000, 123_456_789).unwrap();
        assert_eq!(
            timestamp_value(t),
            json!({ "timestampValue": "2026-09-21T14:13:20.123456Z" })
        );
        let doc = to_doc(&json!({
            "name": "projects/fake-project/databases/(default)/documents/c/d",
            "fields": { "s": string_value("x"), "n": integer_value(-7), "m": integer_value(2) },
            "updateTime": "2026-10-03T00:00:00.000001Z",
        }));
        assert_eq!(doc.string("s"), Some("x"));
        assert_eq!(doc.integer("n"), Some(-7));
        assert_eq!(
            (doc.string("n"), doc.integer("s"), doc.integer("absent")),
            (None, None, None)
        );
        let mut ints: Vec<_> = doc.integers().collect();
        ints.sort();
        assert_eq!(ints, [("m", 2), ("n", -7)]);
        assert_eq!(doc.update_time, "2026-10-03T00:00:00.000001Z");
    }
}
