//! Fixed operator queries. A missing or stale sample never means a healthy zero.
use super::{
    admin_monitoring::{AdminMonitoring, CAPABILITIES_TTL, PAGE_TTL, PAGE_WAIT},
    refresh_cache::Cached,
};
use anyhow::{ensure, Context};
use futures::{stream, StreamExt};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Panel {
    Keymeld,
    Services,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Sample {
    pub metric: BTreeMap<String, String>,
    pub value: (f64, String),
}
impl Sample {
    pub fn number(&self) -> Option<f64> {
        self.value.1.parse::<f64>().ok().filter(|v| v.is_finite())
    }
    pub fn fresh_number(&self, now: f64) -> Option<f64> {
        (self.value.0.is_finite() && now - self.value.0 <= 120.0 && self.value.0 <= now + 30.0)
            .then(|| self.number())
            .flatten()
    }
}
pub type Snapshot = Vec<Option<Vec<Sample>>>;
pub struct Signal {
    pub title: &'static str,
    pub query: &'static str,
    pub unit: &'static str,
    pub review: fn(f64) -> bool,
    pub help: &'static str,
    pub link: &'static str,
}
fn nonzero(v: f64) -> bool {
    v > 0.0
}
fn not_one(v: f64) -> bool {
    v != 1.0
}
fn hour(v: f64) -> bool {
    v > 3600.0
}
fn half_hour(v: f64) -> bool {
    v > 1800.0
}
fn ten_minutes(v: f64) -> bool {
    v > 600.0
}
fn day(v: f64) -> bool {
    v > 93600.0
}
fn informational(_: f64) -> bool {
    false
}

pub const SERVICES: &[Signal] = &[
 Signal { title: "Service probes", query: "probe_success{job=\"app-health\",app=~\"coordinator|oracle|keymeld\"}", unit: "", review: not_one, help: "1 means the probe passed. A failed probe needs the service log and its dependencies checked.", link: "/admin/operations?sort=created_desc" },
 Signal { title: "Weather coverage blocks", query: "oracle_events_blocked_on_source_coverage{job=\"oracle\"}", unit: " events", review: nonzero, help: "Inspect the event's missing station coverage and signed expiry. Missing source data must not be replaced with a guessed outcome.", link: "/admin/operations?show=attention&sort=created_desc" },
 Signal { title: "Oldest attestable event", query: "oracle_oldest_attestable_event_age_seconds{job=\"oracle\"}", unit: " s", review: |v| v > 7200.0, help: "Over two hours needs investigation. Source-blocked events are counted separately.", link: "/admin/operations?show=attention&sort=created_desc" },
 Signal { title: "Oracle ingestion age", query: "time() - oracle_last_etl_completed_timestamp_seconds{job=\"oracle\"}", unit: " s", review: half_hour, help: "Over 30 minutes: inspect the Oracle ETL and upstream weather requests.", link: "/admin/competition" },
 Signal { title: "Stopped coordinator workers", query: "1 - coordinator_background_thread_up{job=\"coordinator\"}", unit: "", review: nonzero, help: "A value of 1 means that worker is down. Check its latest log before restarting anything.", link: "/admin/operations?sort=created_desc" },
 Signal { title: "Lightning and Ark subscriptions", query: "{job=\"coordinator\",__name__=~\"coordinator_ln_invoice_subscription_up|coordinator_ln_payment_subscription_up|coordinator_escrow_subscription_up\"}", unit: "", review: not_one, help: "0 means updates are not confirmed connected. Polling can still advance payments; verify the specific ticket before retrying. The Ark subscription is opened only while an escrow is pending, and reads connected while none is.", link: "/admin/funds" },
 Signal { title: "Oldest open payout", query: "coordinator_payout_job_oldest_open_age_seconds{job=\"coordinator\"}", unit: " s", review: hour, help: "Over one hour: inspect the payout attempt, payment hash, retry time, and signed timelocks.", link: "/admin/funds" },
 Signal { title: "Retrying payout jobs", query: "coordinator_payout_jobs_retrying{job=\"coordinator\"}", unit: " jobs", review: nonzero, help: "Open the entry trace. Reconcile sender status before another payment attempt.", link: "/admin/funds" },
 Signal { title: "Database replication failures", query: "homelab_litestream_failed{app=~\"coordinator|keymeld|oracle\"}", unit: "", review: nonzero, help: "1 means replication failed. Check the app's Litestream log and storage credentials before recovery work.", link: "/admin/operations" },
 Signal { title: "Database replica age", query: "time() - homelab_litestream_last_success_timestamp_seconds{app=~\"coordinator|keymeld|oracle\"}", unit: " s", review: ten_minutes, help: "Over ten minutes needs review. A fresh replica does not prove a restore or include every wallet and witness file.", link: "/admin/operations" },
 Signal { title: "Application backup age", query: "time() - homelab_backup_last_success_timestamp_seconds{role=\"apps\"}", unit: " s", review: day, help: "Over 26 hours: inspect the export and Restic jobs. Preserve the current witness ledger during recovery.", link: "/admin/operations" },
];
pub const KEYMELD: &[Signal] = &[
 Signal { title: "Gateway scrape", query: "up{job=\"keymeld\"}", unit: "", review: not_one, help: "1 means Prometheus reached the gateway's metrics endpoint.", link: "/admin/services" },
 Signal { title: "Enclave health", query: "keymeld_enclave_health{job=\"keymeld\"}", unit: "", review: not_one, help: "Check the gateway connection and enclave log for an unhealthy enclave.", link: "/admin/keymeld" },
 Signal { title: "Confidential relay failures (1h)", query: "sum by(enclave_id) (increase(keymeld_confidential_relay_total{job=\"keymeld\",result=\"transport_error\"}[1h]))", unit: "", review: nonzero, help: "Transport failures can block registration or signing. Encrypted responses can also contain application errors, visible in the entry trace.", link: "/admin/funds" },
 Signal { title: "Confidential responses (1h)", query: "sum by(enclave_id) (increase(keymeld_confidential_relay_total{job=\"keymeld\",result=\"response\"}[1h]))", unit: "", review: informational, help: "Responses prove transport activity, not successful signing or payment.", link: "/admin/funds" },
];
pub const ENCLAVE_QUERY: &str = "{job=\"keymeld\",__name__=~\"keymeld_enclave_health|keymeld_enclave_public_info|keymeld_enclave_deployment_info|keymeld_enclave_connection_active|keymeld_enclave_connection_avg_load|keymeld_enclave_failure_rate_percent\"}";
impl Panel {
    pub fn signals(self) -> &'static [Signal] {
        match self {
            Self::Services => SERVICES,
            Self::Keymeld => KEYMELD,
        }
    }
}

impl AdminMonitoring {
    pub async fn read_signals(self: &Arc<Self>, panel: Panel) -> Cached<Snapshot> {
        self.read_signals_within(panel, PAGE_TTL, PAGE_WAIT).await
    }

    pub(super) async fn read_signals_within(
        self: &Arc<Self>,
        panel: Panel,
        ttl: Duration,
        wait: Duration,
    ) -> Cached<Snapshot> {
        if self.client.is_none() {
            return Cached {
                latest: None,
                refreshing: false,
            };
        }
        let service = self.clone();
        self.signals
            .get_fresh(panel, ttl, wait, move || async move {
                let client = service
                    .client
                    .as_ref()
                    .context("Monitoring is not configured")?;
                let mut queries: Vec<_> = panel.signals().iter().map(|s| s.query).collect();
                if panel == Panel::Keymeld {
                    queries.push(ENCLAVE_QUERY);
                }
                let requests: Vec<_> = queries
                    .into_iter()
                    .map(|query| client.get(&service.query_url).query(&[("query", query)]))
                    .collect();
                let snapshot: Snapshot = tokio::time::timeout(
                    Duration::from_secs(8),
                    stream::iter(requests)
                        .map(|request| async move {
                            match query_samples(request).await {
                                Ok(samples) => Some(samples),
                                Err(error) => {
                                    log::warn!("Admin Grafana request failed: {error:#}");
                                    None
                                }
                            }
                        })
                        .buffered(3)
                        .collect(),
                )
                .await?;
                ensure!(
                    snapshot.iter().any(Option::is_some),
                    "Grafana did not answer any signal query"
                );
                Ok(snapshot)
            })
            .await
    }
    /// The last capability check, refreshed in the background. A page waits only
    /// when there is no check yet.
    pub async fn read_capabilities(
        self: &Arc<Self>,
        coordinator: Arc<crate::domain::Coordinator>,
    ) -> Cached<crate::infra::keymeld::PayoutCapabilities> {
        self.read_capabilities_within(coordinator, CAPABILITIES_TTL, PAGE_WAIT)
            .await
    }

    pub(super) async fn read_capabilities_within(
        self: &Arc<Self>,
        coordinator: Arc<crate::domain::Coordinator>,
        ttl: Duration,
        wait: Duration,
    ) -> Cached<crate::infra::keymeld::PayoutCapabilities> {
        self.capabilities
            .get((), ttl, wait, move || async move {
                tokio::time::timeout(
                    Duration::from_secs(8),
                    coordinator.admin_keymeld_capabilities(),
                )
                .await?
            })
            .await
    }
    pub fn grafana_link(&self, keymeld: bool) -> Option<String> {
        self.client.as_ref()?;
        Some(if keymeld {
            self.dashboard_url
                .replace("homelab-app-coordinator", "homelab-app-keymeld")
        } else {
            self.dashboard_url.clone()
        })
    }
}
async fn query_samples(request: reqwest::RequestBuilder) -> anyhow::Result<Vec<Sample>> {
    let mut response = request.send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 1024 * 1024,
            "Monitoring response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let response: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        response["status"] == "success" && response["data"]["resultType"] == "vector",
        "Monitoring query failed"
    );
    let samples: Vec<Sample> = serde_json::from_value(response["data"]["result"].clone())?;
    ensure!(samples.len() <= 256, "Too many monitoring samples");
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_missing_and_nonfinite_readings_do_not_become_healthy() {
        let sample = |at, value: &str| Sample {
            metric: BTreeMap::new(),
            value: (at, value.into()),
        };
        assert_eq!(sample(100.0, "0").fresh_number(150.0), Some(0.0));
        assert_eq!(sample(100.0, "0").fresh_number(221.0), None);
        assert_eq!(sample(200.0, "0").fresh_number(100.0), None);
        assert_eq!(sample(100.0, "NaN").fresh_number(100.0), None);
        assert_eq!(sample(f64::NAN, "0").fresh_number(100.0), None);
    }
}
