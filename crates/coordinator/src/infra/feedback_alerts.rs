//! Alerts for new feedback, pushed to ntfy. One push at most every [`PUSH_EVERY`]; messages
//! that arrive in between are folded into the next push as "+N more". A failed push leaves
//! its messages unnotified, so the next tick tries again, until they are a day old.
//!
//! Email goes through ntfy too: with `notify_email` set, each push carries ntfy's `Email`
//! header. A server without email refuses that header; the push is sent again without it,
//! and later pushes leave it off.

use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, ensure, Context};
use log::{info, warn};
use time::OffsetDateTime;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use zeroize::Zeroizing;

use crate::{
    config::FeedbackSettings,
    domain::{
        feedback::{excerpt, FeedbackRow, FeedbackStore, DUPLICATE_WINDOW_SECS},
        WorkerLeases,
    },
};

/// The shortest time between two pushes.
pub const PUSH_EVERY: Duration = Duration::from_secs(120);
/// How often unnotified messages are looked for.
const TICK: Duration = Duration::from_secs(15);
/// Messages older than this are not pushed any more.
const GIVE_UP_AFTER_SECS: i64 = DUPLICATE_WINDOW_SECS;
/// Characters of a message a push carries.
const EXCERPT_CHARS: usize = 140;
/// Messages read for one push; the rest are counted in "+N more" next time.
const BATCH: i64 = 100;
const TITLE: &str = "5day4cast feedback";
const LEASE: &str = "feedback-alerts";

/// What one push says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Push {
    pub body: String,
    pub click: Option<String>,
}

/// The push for `rows`, oldest first: the first message's excerpt and page, and how many
/// more arrived.
pub fn push_for(rows: &[FeedbackRow], admin_url: Option<&str>) -> Option<Push> {
    let first = rows.first()?;
    let mut body = excerpt(&first.message, EXCERPT_CHARS);
    body.push('\n');
    body.push_str(first.page.as_deref().unwrap_or("(page unknown)"));
    if rows.len() > 1 {
        body.push_str(&format!("\n+{} more", rows.len() - 1));
    }
    Some(Push {
        body,
        click: admin_url
            .map(|base| format!("{}/admin/feedback/{}", base.trim_end_matches('/'), first.id)),
    })
}

pub struct FeedbackAlerts {
    client: reqwest::Client,
    topic_url: String,
    token: Option<Zeroizing<String>>,
    email: Option<String>,
    admin_url: Option<String>,
    /// The server refused the `Email` header once; it is left off from then on.
    email_refused: AtomicBool,
}

impl FeedbackAlerts {
    /// The alerts `settings` describe, or `None` when feedback or ntfy is not configured.
    pub fn from_settings(settings: &FeedbackSettings) -> anyhow::Result<Option<Self>> {
        let Some(ntfy_url) = settings.ntfy_url.as_deref().filter(|_| settings.enabled) else {
            return Ok(None);
        };
        let token = match &settings.ntfy_token_file {
            Some(path) => {
                let mut token = Zeroizing::new(String::new());
                std::fs::File::open(path)
                    .context("Opening the ntfy token file")?
                    .take(4097)
                    .read_to_string(&mut token)?;
                ensure!(
                    token.len() <= 4096 && !token.trim().is_empty(),
                    "The ntfy token file must contain a token of at most 4096 bytes"
                );
                Some(Zeroizing::new(token.trim().to_owned()))
            }
            None => None,
        };
        Ok(Some(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()?,
            topic_url: format!("{}/{}", ntfy_url.trim_end_matches('/'), settings.ntfy_topic),
            token,
            email: settings.notify_email.clone(),
            admin_url: settings.admin_url.clone(),
            email_refused: AtomicBool::new(false),
        }))
    }

    /// Send one push, with the `Email` header unless the server refused it before.
    pub async fn send(&self, push: &Push) -> anyhow::Result<()> {
        let email = self
            .email
            .as_deref()
            .filter(|_| !self.email_refused.load(Ordering::Relaxed));
        match self.post(push, email).await {
            Err(Refusal::Email) => {
                if !self.email_refused.swap(true, Ordering::Relaxed) {
                    warn!("ntfy refused the Email header (is email set up on the server?); alerts go without it");
                }
                self.post(push, None).await.map_err(Refusal::into_error)
            }
            result => result.map_err(Refusal::into_error),
        }
    }

    async fn post(&self, push: &Push, email: Option<&str>) -> Result<(), Refusal> {
        let mut request = self
            .client
            .post(&self.topic_url)
            .header("Title", TITLE)
            .header("Tags", "speech_balloon")
            .body(push.body.clone());
        if let Some(token) = &self.token {
            let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", **token))
                .map_err(|e| Refusal::Other(anyhow!("ntfy token: {e}")))?;
            auth.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, auth);
        }
        if let Some(click) = &push.click {
            request = request.header("Click", click);
        }
        if let Some(email) = email {
            request = request.header("Email", email);
        }
        let response = request
            .send()
            .await
            .map_err(|e| Refusal::Other(anyhow!("ntfy: {}", e.without_url())))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.bytes().await.unwrap_or_default();
        let body = &body[..body.len().min(4096)];
        if email.is_some() && email_refused(status.as_u16(), body) {
            return Err(Refusal::Email);
        }
        Err(Refusal::Other(anyhow!("ntfy answered {status}")))
    }

    /// Push unnotified messages every [`TICK`], at most once per [`PUSH_EVERY`], on the
    /// coordinator that holds the alerts lease.
    pub fn spawn(
        self: Arc<Self>,
        store: FeedbackStore,
        leases: Arc<WorkerLeases>,
        tracker: &TaskTracker,
        cancel: CancellationToken,
    ) {
        tracker.spawn(async move {
            let mut pushed_at: Option<tokio::time::Instant> = None;
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                if pushed_at.is_some_and(|at| at.elapsed() < PUSH_EVERY) {
                    continue;
                }
                let alerts = self.clone();
                let store = store.clone();
                match leases.tick(LEASE, alerts.push_pending(&store)).await {
                    Some(Ok(true)) => pushed_at = Some(tokio::time::Instant::now()),
                    Some(Ok(false)) | None => {}
                    Some(Err(error)) => warn!("Feedback alert not sent; will retry: {error:#}"),
                }
            }
            leases.release(LEASE).await;
        });
    }

    /// Push what is unnotified, if anything. Answers whether a push went out.
    async fn push_pending(&self, store: &FeedbackStore) -> anyhow::Result<bool> {
        let now = OffsetDateTime::now_utc();
        let rows = store
            .unnotified(now - time::Duration::seconds(GIVE_UP_AFTER_SECS), BATCH)
            .await?;
        let Some(push) = push_for(&rows, self.admin_url.as_deref()) else {
            return Ok(false);
        };
        self.send(&push).await?;
        let count = rows.len();
        store
            .mark_notified(rows.into_iter().map(|row| row.id).collect(), now)
            .await?;
        info!("Feedback alert sent for {count} message(s)");
        Ok(true)
    }
}

enum Refusal {
    /// The server does not send email.
    Email,
    Other(anyhow::Error),
}

impl Refusal {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Email => anyhow!("ntfy refused the push"),
            Self::Other(error) => error,
        }
    }
}

/// Whether ntfy refused a push for its `Email` header: 400 with an error code 400xx, as ntfy
/// answers when email is not set up.
fn email_refused(status: u16, body: &[u8]) -> bool {
    if status != 400 {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|error| error["code"].as_u64())
        .is_some_and(|code| (40000..40100).contains(&code))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Router,
    };
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;
    use crate::domain::feedback::FeedbackStatus;

    fn row(message: &str, page: Option<&str>) -> FeedbackRow {
        FeedbackRow {
            id: Uuid::parse_str("0199c1a2-0000-7000-8000-000000000001").unwrap(),
            created_at: datetime!(2026-10-07 10:00 UTC),
            message: message.into(),
            contact: Some("someone@example.com".into()),
            page: page.map(Into::into),
            rid: None,
            sid: None,
            pubkey: None,
            ip: Some("203.0.113.9".into()),
            user_agent: None,
            status: FeedbackStatus::New,
            operator_note: None,
            notified_at: None,
        }
    }

    #[test]
    fn a_push_has_the_first_excerpt_its_page_and_how_many_more() {
        assert_eq!(push_for(&[], None), None);
        let one = push_for(
            &[row("The map\nis slow", Some("/competitions"))],
            Some("https://admin.example.com:9443/"),
        )
        .unwrap();
        assert_eq!(one.body, "The map is slow\n/competitions");
        assert_eq!(
            one.click.as_deref(),
            Some("https://admin.example.com:9443/admin/feedback/0199c1a2-0000-7000-8000-000000000001")
        );
        let long = "x".repeat(500);
        let many = push_for(&[row(&long, None), row("b", None), row("c", None)], None).unwrap();
        let lines: Vec<_> = many.body.lines().collect();
        assert_eq!(lines[0].chars().count(), EXCERPT_CHARS);
        assert_eq!(lines[1], "(page unknown)");
        assert_eq!(lines[2], "+2 more");
        assert_eq!(many.click, None);
        // The contact and the address never go in a push.
        assert!(!one.body.contains("example.com") && !one.body.contains("203.0.113.9"));
    }

    #[test]
    fn only_a_400xx_refusal_means_email_is_not_set_up() {
        assert!(email_refused(
            400,
            br#"{"code":40001,"http":400,"error":"e-mail notifications are not enabled"}"#
        ));
        assert!(!email_refused(400, br#"{"code":40401}"#));
        assert!(!email_refused(403, br#"{"code":40301}"#));
        assert!(!email_refused(400, b"not json"));
    }

    /// Headers of each push the stub received, and whether it refuses the Email header.
    #[derive(Clone, Default)]
    struct Stub {
        seen: Arc<Mutex<Vec<(HeaderMap, String)>>>,
        refuse_email: bool,
    }

    async fn publish(
        State(stub): State<Stub>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, &'static str) {
        let email = headers.contains_key("email");
        stub.seen.lock().unwrap().push((headers, body));
        if email && stub.refuse_email {
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":40001,"http":400,"error":"e-mail notifications are not enabled"}"#,
            )
        } else {
            (StatusCode::OK, "{}")
        }
    }

    async fn stub(refuse_email: bool) -> (Stub, String) {
        let stub = Stub {
            refuse_email,
            ..Stub::default()
        };
        let app = Router::new()
            .route("/feedback", post(publish))
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (stub, url)
    }

    fn alerts(url: &str, token_file: Option<&std::path::Path>) -> FeedbackAlerts {
        FeedbackAlerts::from_settings(&FeedbackSettings {
            enabled: true,
            ntfy_url: Some(url.into()),
            ntfy_token_file: token_file.map(|path| path.display().to_string()),
            notify_email: Some("ops@example.com".into()),
            admin_url: Some("https://admin.example.com".into()),
            ..FeedbackSettings::default()
        })
        .unwrap()
        .unwrap()
    }

    #[tokio::test]
    async fn pushes_carry_the_token_title_click_tags_and_email() {
        let (stub, url) = stub(false).await;
        let token = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(token.path(), "tk_secret\n").unwrap();
        let alerts = alerts(&url, Some(token.path()));
        let push = push_for(&[row("hello", Some("/"))], alerts.admin_url.as_deref()).unwrap();
        alerts.send(&push).await.unwrap();
        let seen = stub.seen.lock().unwrap();
        let (headers, body) = &seen[0];
        assert_eq!(headers["authorization"], "Bearer tk_secret");
        assert_eq!(headers["title"], "5day4cast feedback");
        assert_eq!(headers["tags"], "speech_balloon");
        assert_eq!(headers["email"], "ops@example.com");
        assert!(headers["click"]
            .to_str()
            .unwrap()
            .starts_with("https://admin.example.com/admin/feedback/"));
        assert_eq!(body, "hello\n/");
    }

    #[tokio::test]
    async fn a_server_without_email_gets_the_push_without_the_header() {
        let (stub, url) = stub(true).await;
        let alerts = alerts(&url, None);
        let push = push_for(&[row("hello", Some("/"))], None).unwrap();
        alerts.send(&push).await.unwrap();
        {
            let seen = stub.seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert!(seen[0].0.contains_key("email"));
            assert!(!seen[1].0.contains_key("email"));
        }
        assert!(alerts.email_refused.load(Ordering::Relaxed));
        // Later pushes go without it at once.
        alerts.send(&push).await.unwrap();
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(!seen[2].0.contains_key("email"));
    }

    #[test]
    fn alerts_are_off_without_feedback_or_ntfy() {
        assert!(FeedbackAlerts::from_settings(&FeedbackSettings::default())
            .unwrap()
            .is_none());
        assert!(FeedbackAlerts::from_settings(&FeedbackSettings {
            enabled: true,
            ..FeedbackSettings::default()
        })
        .unwrap()
        .is_none());
        assert!(FeedbackAlerts::from_settings(&FeedbackSettings {
            enabled: true,
            ntfy_url: Some("http://127.0.0.1:1".into()),
            ntfy_token_file: Some("/nonexistent/ntfy_token".into()),
            ..FeedbackSettings::default()
        })
        .is_err());
    }
}
