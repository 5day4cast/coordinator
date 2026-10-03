//! Optional, bounded Grafana reads. Credentials and queries stay on the server.
use std::{io::Read, sync::Arc, time::Duration};

use anyhow::{ensure, Context};
use futures::{stream, StreamExt};
use maud::{html, Markup};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use zeroize::Zeroizing;

use super::refresh_cache::{Cached, RefreshCache};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitoringSettings {
    pub grafana_url: String,
    pub datasource_uid: String,
    pub token_file: String,
}

/// Pages wait for a refresh of values older than this, for up to [`PAGE_WAIT`].
pub(super) const PAGE_TTL: Duration = Duration::from_secs(60);
pub(super) const PAGE_WAIT: Duration = Duration::from_secs(9);
/// The background refresh renews values this old, ahead of [`PAGE_TTL`].
pub(super) const PREWARM_TTL: Duration = Duration::from_secs(40);
/// Capability support is configuration; it is rechecked every ten minutes.
pub const CAPABILITIES_TTL: Duration = Duration::from_secs(600);

const QUERIES: &[(&str, &str, &str)] = &[
    (
        "Coordinator scrape",
        "min(up{job=\"coordinator\"})",
        "1 = reachable, 0 = scrape failed",
    ),
    (
        "Open payout jobs",
        "sum(coordinator_payout_jobs_open{job=\"coordinator\"})",
        "Outstanding jobs, not distinct unpaid players",
    ),
    (
        "Retrying payouts",
        "sum(coordinator_payout_jobs_retrying{job=\"coordinator\"})",
        "Jobs scheduled to retry",
    ),
    (
        "Oldest open payout",
        "max(coordinator_payout_job_oldest_open_age_seconds{job=\"coordinator\"})",
        "Seconds since the oldest open job was created",
    ),
    (
        "Stopped workers",
        "sum(1 - coordinator_background_thread_up{job=\"coordinator\"})",
        "Recorded background workers that are down",
    ),
    (
        "Payout data age",
        "time() - min(timestamp(coordinator_payout_jobs_open{job=\"coordinator\"}))",
        "Seconds since the oldest included sample; check scrape status too",
    ),
];

#[derive(Debug)]
pub struct Metric {
    value: Option<f64>,
}

pub struct AdminMonitoring {
    configuration_error: bool,
    pub(super) client: Option<reqwest::Client>,
    pub(super) query_url: String,
    pub(super) dashboard_url: String,
    pub(super) signals:
        Arc<RefreshCache<super::admin_signals::Panel, super::admin_signals::Snapshot>>,
    pub(super) capabilities: Arc<RefreshCache<(), crate::infra::keymeld::PayoutCapabilities>>,
    cache: Arc<RefreshCache<(), Vec<Metric>>>,
}

impl AdminMonitoring {
    pub fn from_settings(settings: Option<&MonitoringSettings>) -> Self {
        let environment = match (
            std::env::var("COORDINATOR_GRAFANA_URL"),
            std::env::var("COORDINATOR_GRAFANA_DATASOURCE"),
            std::env::var("COORDINATOR_GRAFANA_TOKEN_FILE"),
        ) {
            (Ok(grafana_url), Ok(datasource_uid), Ok(token_file)) => Some(MonitoringSettings {
                grafana_url,
                datasource_uid,
                token_file,
            }),
            _ => None,
        };
        match Self::new(settings.or(environment.as_ref())) {
            Ok(service) => service,
            Err(error) => {
                log::warn!("Admin monitoring is unavailable: {error}");
                let mut service = Self::new(None).expect("unconfigured monitoring needs no I/O");
                service.configuration_error = true;
                service
            }
        }
    }

    pub fn new(settings: Option<&MonitoringSettings>) -> anyhow::Result<Self> {
        let mut service = Self {
            configuration_error: false,
            client: None,
            query_url: String::new(),
            dashboard_url: String::new(),
            cache: Arc::new(RefreshCache::new()),
            signals: Arc::new(RefreshCache::new()),
            capabilities: Arc::new(RefreshCache::new()),
        };
        let Some(settings) = settings else {
            return Ok(service);
        };
        let url = reqwest::Url::parse(&settings.grafana_url)?;
        ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))),
            "Grafana requires HTTPS outside loopback"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Grafana URL must not contain credentials, a query, or a fragment"
        );
        ensure!(
            !settings.datasource_uid.is_empty()
                && settings.datasource_uid.len() <= 64
                && settings
                    .datasource_uid
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid Grafana datasource UID"
        );
        let mut token = Zeroizing::new(String::new());
        std::fs::File::open(&settings.token_file)
            .context("Opening the Grafana token file")?
            .take(4097)
            .read_to_string(&mut token)?;
        ensure!(
            token.len() <= 4096 && !token.trim().is_empty(),
            "Grafana token file must contain a token of at most 4096 bytes"
        );
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))?;
        auth.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        service.client = Some(
            reqwest::Client::builder()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(4))
                .build()?,
        );
        let base = settings.grafana_url.trim_end_matches('/');
        service.query_url = format!(
            "{base}/api/datasources/proxy/uid/{}/api/v1/query",
            settings.datasource_uid
        );
        service.dashboard_url = format!("{base}/d/homelab-app-coordinator?from=now-6h&to=now");
        Ok(service)
    }

    pub async fn read(self: &Arc<Self>) -> Cached<Vec<Metric>> {
        self.read_within(PAGE_TTL, PAGE_WAIT).await
    }

    /// Keep the operator cards fresh in the background, so opening a page does not
    /// wait for Grafana. Values are refreshed before pages would count them overdue.
    /// Keymeld capabilities use real enclave requests, so they are checked less often.
    pub fn spawn_refresher(
        self: &Arc<Self>,
        tracker: &tokio_util::task::TaskTracker,
        cancel: tokio_util::sync::CancellationToken,
        coordinator: Arc<crate::domain::Coordinator>,
    ) {
        if self.client.is_none() {
            return;
        }
        let service = self.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        // A zero wait starts any due refresh and returns at once.
                        service.read_within(PREWARM_TTL, Duration::ZERO).await;
                        for panel in [super::admin_signals::Panel::Services, super::admin_signals::Panel::Keymeld] {
                            service.read_signals_within(panel, PREWARM_TTL, Duration::ZERO).await;
                        }
                        if coordinator.is_keymeld_enabled() {
                            service
                                .read_capabilities_within(coordinator.clone(), CAPABILITIES_TTL, Duration::ZERO)
                                .await;
                        }
                    }
                }
            }
        });
    }

    async fn read_within(self: &Arc<Self>, ttl: Duration, wait: Duration) -> Cached<Vec<Metric>> {
        if self.client.is_none() {
            return Cached {
                latest: None,
                refreshing: false,
            };
        }
        let service = self.clone();
        self.cache
            .get_fresh((), ttl, wait, move || async move {
                let client = service
                    .client
                    .as_ref()
                    .context("Grafana is not configured")?;
                let requests: Vec<_> = QUERIES
                    .iter()
                    .map(|(_, query, _)| client.get(&service.query_url).query(&[("query", query)]))
                    .collect();
                let metrics = tokio::time::timeout(
                    Duration::from_secs(8),
                    stream::iter(requests)
                        .map(|request| async move {
                            let value = async {
                                let response: serde_json::Value =
                                    request.send().await?.error_for_status()?.json().await?;
                                metric_value(&response)
                                    .context("Grafana returned no single finite sample")
                            }
                            .await
                            .ok();
                            Metric { value }
                        })
                        .buffered(3)
                        .collect::<Vec<_>>(),
                )
                .await?;
                ensure!(
                    metrics.iter().any(|metric| metric.value.is_some()),
                    "Grafana did not answer any operation metric query"
                );
                Ok(metrics)
            })
            .await
    }

    pub fn render(&self, data: &Cached<Vec<Metric>>) -> Markup {
        html! {
            section {
                h2 { "Service signals" }
                @if self.client.is_none() {
                    p.note { @if self.configuration_error { "Grafana is configured but unavailable. Check the service log and token file access." } @else { "Grafana is not connected. Competition records below remain available." } }
                } @else {
                    a href=(&self.dashboard_url) rel="noreferrer" { "Open Grafana" }
                    @if let Some(latest) = &data.latest {
                        p.note { "Queried " (latest.fetched_at.format(&Rfc3339).unwrap_or_default()) "."
                            @if latest.age() > Duration::from_secs(120) { " Cached results are stale." }
                        }
                    } @else { p.note { @if data.refreshing { "Fetching service signals. Reload shortly." } @else { "Monitoring is unavailable." } } }
                    div.metric-grid {
                        @for (index, (label, _, help)) in QUERIES.iter().enumerate() {
                            div.metric { span { (label) }
                                strong { @match data.value().and_then(|metrics| metrics.get(index)).and_then(|m| m.value) {
                                    Some(value) => (format!("{value:.0}")), None => "Unknown",
                                } }
                                p.note { (help) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn metric_value(response: &serde_json::Value) -> Option<f64> {
    if response["status"] != "success" || response["data"]["resultType"] != "vector" {
        return None;
    }
    let samples = response["data"]["result"].as_array()?;
    if samples.len() != 1 {
        return None;
    }
    samples[0]["value"][1]
        .as_str()?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_optional_monitoring_does_not_disable_the_operator_dashboard() {
        let service = AdminMonitoring::from_settings(Some(&MonitoringSettings {
            grafana_url: "not a URL".into(),
            datasource_uid: "prometheus".into(),
            token_file: "/unused".into(),
        }));
        let html = service
            .render(&Cached {
                latest: None,
                refreshing: false,
            })
            .into_string();
        assert!(html.contains("configured but unavailable"));
        assert!(service.client.is_none());
    }
    #[test]
    fn absent_and_nonfinite_metrics_never_become_healthy_zeroes() {
        for samples in [
            serde_json::json!([]),
            serde_json::json!([{ "value": [1, "NaN"] }]),
            serde_json::json!([{ "value": [1, "1"] }, { "value": [1, "2"] }]),
        ] {
            assert_eq!(
                metric_value(
                    &serde_json::json!({ "status": "success", "data": { "resultType": "vector", "result": samples } })
                ),
                None
            );
        }
        assert_eq!(
            metric_value(
                &serde_json::json!({ "status": "success", "data": { "resultType": "vector", "result": [{ "value": [1, "0"] }] } })
            ),
            Some(0.0)
        );
    }
}
