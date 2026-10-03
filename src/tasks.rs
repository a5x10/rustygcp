//! Cloud Tasks: create an HTTP task that calls back with a Google OIDC token
//! (the receiver checks it with [`crate::Verifier`]).

use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::{Auth, Error, Result, ok};

pub struct Tasks {
    auth: Auth,
    /// `projects/<p>/locations/<l>/queues/<q>`
    queue: String,
    root: String,
}

/// A `POST` of a JSON body to `url`.
#[derive(Debug, Clone)]
pub struct HttpTask<'a> {
    pub url: &'a str,
    pub body: &'a [u8],
    /// The service account Cloud Tasks signs the OIDC token as. The caller
    /// needs `iam.serviceAccounts.actAs` on it.
    pub oidc_email: &'a str,
    /// The token's `aud`.
    pub audience: &'a str,
    /// The task id (`[A-Za-z0-9_-]`, up to 500): a second create with the same
    /// id is refused for as long as the task exists and about an hour after,
    /// which is the dedup. `None` = Cloud Tasks picks one.
    pub name: Option<&'a str>,
    /// Not before this time; `None` = now.
    pub at: Option<DateTime<Utc>>,
    /// How long Cloud Tasks waits for the answer before it counts the attempt
    /// as failed; `None` = its default of 10 minutes, the most it allows is 30.
    pub dispatch_deadline: Option<Duration>,
}

impl Tasks {
    pub fn new(auth: &Auth, project: &str, location: &str, queue: &str) -> Self {
        Self {
            auth: auth.clone(),
            queue: format!("projects/{project}/locations/{location}/queues/{queue}"),
            root: "https://cloudtasks.googleapis.com/v2".into(),
        }
    }

    /// `false` when a task with this name already exists (or did within the
    /// dedup window): nothing was created.
    pub async fn create(&self, task: &HttpTask<'_>) -> Result<bool> {
        let req = self
            .auth
            .http()
            .post(format!("{}/{}/tasks", self.root, self.queue))
            .header("authorization", self.auth.bearer().await?)
            .json(&self.task_json(task));
        match ok(req.send().await?).await {
            Ok(_) => Ok(true),
            Err(Error::Status { status: 409, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn task_json(&self, t: &HttpTask<'_>) -> Value {
        let mut task = json!({
            "httpRequest": {
                "url": t.url,
                "httpMethod": "POST",
                "headers": { "Content-Type": "application/json" },
                "body": base64::engine::general_purpose::STANDARD.encode(t.body),
                "oidcToken": { "serviceAccountEmail": t.oidc_email, "audience": t.audience },
            },
        });
        if let Some(name) = t.name {
            task["name"] = json!(format!("{}/tasks/{name}", self.queue));
        }
        if let Some(at) = t.at {
            task["scheduleTime"] = json!(at.to_rfc3339_opts(SecondsFormat::Micros, true));
        }
        if let Some(d) = t.dispatch_deadline {
            task["dispatchDeadline"] = json!(format!("{}s", d.as_secs()));
        }
        json!({ "task": task })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fake;

    const TASK: HttpTask<'static> = HttpTask {
        url: "https://svc.example/internal/phase",
        body: br#"{"game":"g1","phase":3}"#,
        oidc_email: "tasks@fake-project.iam.gserviceaccount.com",
        audience: "https://svc.example",
        name: None,
        at: None,
        dispatch_deadline: None,
    };

    #[tokio::test]
    async fn create_posts_the_task_and_reports_a_duplicate_name() {
        let server = fake(vec![
            (200, "{}".into()),
            (
                409,
                r#"{"error":{"status":"ALREADY_EXISTS","message":"exists"}}"#.into(),
            ),
            (
                403,
                r#"{"error":{"status":"PERMISSION_DENIED","message":"no actAs"}}"#.into(),
            ),
        ])
        .await;
        let mut tasks = Tasks::new(
            &Auth::static_token("fake-token"),
            "fake-project",
            "europe-west1",
            "jobs",
        );
        tasks.root = server.url.clone();
        let named = HttpTask {
            name: Some("g1-phase-3"),
            at: DateTime::from_timestamp(1_790_000_000, 0),
            dispatch_deadline: Some(Duration::from_secs(900)),
            ..TASK
        };

        assert!(tasks.create(&named).await.unwrap());
        assert!(!tasks.create(&named).await.unwrap());
        let denied = tasks.create(&TASK).await.unwrap_err();
        assert_eq!(denied.to_string(), "403 PERMISSION_DENIED: no actAs");

        let seen = server.seen();
        let queue = "projects/fake-project/locations/europe-west1/queues/jobs";
        assert!(
            seen[0].starts_with(&format!("POST /{queue}/tasks HTTP/1.1\r\n")),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains("authorization: Bearer fake-token\r\n"));
        assert_eq!(
            server.body(0),
            json!({ "task": {
                "name": format!("{queue}/tasks/g1-phase-3"),
                "scheduleTime": "2026-09-21T14:13:20.000000Z",
                "dispatchDeadline": "900s",
                "httpRequest": {
                    "url": "https://svc.example/internal/phase",
                    "httpMethod": "POST",
                    "headers": { "Content-Type": "application/json" },
                    "body": "eyJnYW1lIjoiZzEiLCJwaGFzZSI6M30=",
                    "oidcToken": {
                        "serviceAccountEmail": "tasks@fake-project.iam.gserviceaccount.com",
                        "audience": "https://svc.example",
                    },
                },
            }})
        );
        // no name, no time, no deadline: Cloud Tasks names it, runs it now, waits 10 minutes
        let unnamed = server.body(2);
        for field in ["name", "scheduleTime", "dispatchDeadline"] {
            assert!(unnamed["task"].get(field).is_none(), "{field}");
        }
    }
}
