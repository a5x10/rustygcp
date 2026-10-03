# rustygcp

The few Google Cloud REST calls a small Cloud Run service needs, async on reqwest + rustls: Firestore documents,
Cloud Storage objects, Cloud Tasks HTTP tasks, the check of a Google-signed OIDC token. No service logic, no generated
SDK. Extracted from a service that keeps its whole state in memory and uses Firestore as the file it is saved to.

```toml
rustygcp = { git = "https://github.com/a5x10/rustygcp", rev = "<commit>" }
```

## API

```rust
use rustygcp::{Auth, Error, Firestore, Gcs, HttpTask, Precondition, Tasks, Verifier, Write};
use rustygcp::firestore::{integer_value, string_value, timestamp_value};

// One Auth per process; clones share the token cache.
// GOOGLE_OAUTH_ACCESS_TOKEN if set (a dev box), else the metadata server (Cloud Run), cached until 60 s before expiry.
let auth = Auth::from_env();

// Firestore. FIRESTORE_EMULATOR_HOST switches to the emulator; FIRESTORE_DATABASE names a database other than (default).
let fs = Firestore::from_env("my-project", &auth);
let doc = fs.get("games/g1").await?;                     // Option<Doc>: raw typed fields + update_time
let all = fs.list("games").await?;                       // Vec<(id, Doc)>, every page
let some = fs.query("games", Some(filter), true, Some(10)).await?;  // raw `where`, order by id, limit
let times = fs.commit(&[                                 // atomic, <= 500 writes; Err(Error::Precondition) = nothing written
    Write::Put { path: "games/g1".into(), fields, pre: Precondition::UpdateTime(doc.update_time) },
    Write::Merge { path: "t/shard-0".into(), fields: one_entry, pre: Precondition::None },  // these fields only
    Write::Increment { path: "spend/2026-10-03".into(), by: vec![("micros".into(), 5)] },
    Write::Delete { path: "games/old".into() },
]).await?;                                               // each write's update_time
let (reads, writes, deletes) = fs.ops.snapshot();        // billed operations since start
// also: Precondition::{None, Exists(bool)}, Doc::{string, integer, integers}, fs.collection_ids(), fs.reference(path)

// Cloud Storage, one bucket, JSON API. Object names may contain `/`.
let gcs = Gcs::new(&auth, "my-bucket");
gcs.put("images/1.png", "image/png", bytes).await?;
let obj = gcs.get("images/1.png").await?;                // Option<Object { bytes, content_type }>
let names = gcs.list("images/").await?;                  // Vec<(name, size)>
let existed = gcs.delete("images/1.png").await?;         // false = there was none

// Cloud Tasks: POST a JSON body to a URL later, signed with an OIDC token.
let tasks = Tasks::new(&auth, "my-project", "europe-west1", "jobs");
let created = tasks.create(&HttpTask {
    url: "https://svc.example/internal/phase",
    body: br#"{"game":"g1","phase":3}"#,
    oidc_email: "tasks@my-project.iam.gserviceaccount.com",  // the caller needs actAs on it
    audience: "https://svc.example",
    name: Some("g1-phase-3"),                            // dedup: false if this name exists or did in the last ~hour
    at: Some(deadline),                                  // chrono DateTime<Utc>; None = now
}).await?;

// The internal route that Cloud Tasks or Cloud Scheduler calls. Fails closed.
let verifier = Verifier::new();                          // one per process: Google's keys are cached for an hour
verifier.check(authorization_header, "https://svc.example", "tasks@my-project.iam.gserviceaccount.com").await?;
// Err(OidcError::Unauthorized) -> 401, Forbidden (a Google token of another account) -> 403, Keys -> 500
```

Errors: `Error::Precondition` (a commit lost its precondition), `Error::Status { status, code, message }` (Google's
error body), `Error::Http` (no usable answer). The default timeout is 10 s, 60 s for Cloud Storage. A `get` that finds
nothing is `None`, not an error. Access tokens never appear in `Debug` output or in errors.

## Tests

`just test` runs rustfmt, clippy and every test. The Firestore tests need the emulator
(`gcloud components install cloud-firestore-emulator`, Java): `scripts/with-firestore-emulator.sh cargo test` starts
one on a free port and stops it; without `FIRESTORE_EMULATOR_HOST` they skip. Cloud Storage, Cloud Tasks, the metadata
token cache and the OIDC check are tested against a local socket that records each request as it arrived on the wire;
the OIDC tests sign with an RSA key generated per run. Nothing in the tests reaches Google.

## License

Apache-2.0.
