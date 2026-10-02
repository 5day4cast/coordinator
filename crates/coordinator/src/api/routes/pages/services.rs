//! Read-only operator dependency views. No signing or payment actions live here.
use super::admin::render_admin_fragment;
use crate::{
    api::admin_auth::AdminCsrf,
    infra::{
        admin_signals::{Panel, Sample, Signal, Snapshot, KEYMELD},
        refresh_cache::Cached,
    },
    startup::AppState,
};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Html,
    Extension,
};
use maud::{html, Markup};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Default, Deserialize)]
pub struct SupportFilter {
    #[serde(default)]
    q: String,
}

pub async fn services_page(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    headers: HeaderMap,
) -> Html<String> {
    let data = state.admin_monitoring.read_signals(Panel::Services).await;
    let content = html! { main.admin-workspace {
        p.eyebrow { "Operator checks" } h1 { "Services" }
        p { "Find the affected entry first, then check the dependency that can block its next step." }
        (lookup(""))
        div.metric-grid {
            article.metric { h2 { "Signing & escrow" } p { "Gateway, enclaves, and the coordinator verifier." } a href="/admin/keymeld" { "Check Keymeld" } }
            article.metric { h2 { "Lightning & Ark" } p { "LND sync, channel liquidity, on-chain balances, and Ark expiry. A ticket's service check gives its current payment and swap status." } a href="/admin/wallet" { "Node & wallets" } " · " a href="/admin/funds" { "Trace a payment" } }
            article.metric { h2 { "Recent competitions" } p { "Separate recent progress from refunds caused by earlier failures." } a href="/admin/operations?sort=created_desc" { "Newest created" } }
        }
        (signals(&state, Panel::Services, &data, "/admin/services", None))
    }};
    render_admin_fragment(&headers, &state, &csrf, "Services", content)
}

pub async fn keymeld_page(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(filter): Query<SupportFilter>,
    headers: HeaderMap,
) -> Html<String> {
    let (data, capabilities, competitions) = tokio::join!(
        state.admin_monitoring.read_signals(Panel::Keymeld),
        state
            .admin_monitoring
            .read_capabilities(state.coordinator.clone()),
        state.coordinator.list_operator_competitions(),
    );
    let competitions = competitions.map(|mut competitions| {
        competitions.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        competitions
    });
    let content = html! { main.admin-workspace {
        p.eyebrow { "Registration · signing · escrow" } h1 { "Keymeld & enclaves" }
        @if let Some(url) = std::env::var("COORDINATOR_KEYMELD_ADMIN_URL").ok().and_then(|u| reqwest::Url::parse(&u).ok()).filter(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none() && u.query().is_none() && u.fragment().is_none()) { p { a href=(url.as_str()) rel="noreferrer" { "Open Keymeld admin" } } }
        (lookup(&filter.q))
        p.note { "The gateway relays confidential traffic. Its session list does not include that work. Use competition records to identify affected entries; transport activity alone cannot prove signing or payout success." }
        (signals(&state, Panel::Keymeld, &data, &keymeld_path(&filter.q), Some(&capabilities)))
        h2 { "Competitions to inspect" }
        p.note { "Newest first. These records have signing in progress or retained signing errors; inclusion does not establish a current Keymeld fault." }
        form.discovery-filters method="get" action="/admin/keymeld" { label { "Competition ID or station" input name="q" value=(&filter.q) maxlength="256"; } button type="submit" { "Filter competitions" } }
        @match competitions {
            Ok(competitions) => {
                @let search = filter.q.trim().to_lowercase();
                @let candidates: Vec<_> = competitions.iter().filter(|c| {
                    let signing = c.contracted_at.is_some() && c.signed_at.is_none() && c.failed_at.is_none() && c.cancelled_at.is_none();
                    let retained = c.errors.iter().any(|e| { let text = format!("{e:?}").to_lowercase(); ["keymeld", "enclave", "verifier", "signing"].iter().any(|term| text.contains(term)) });
                    (signing || retained) && (search.is_empty() || c.id.to_string().contains(&search) || c.event_submission.locations.iter().any(|s| s.to_lowercase().contains(&search)))
                }).collect();
                @if candidates.is_empty() { p { "No matching competitions have signing in progress or retained signing errors." } }
                @else { p.note { "Showing " (candidates.len().min(50)) " of " (candidates.len()) "." }
                    div.scroll { table.ops-table { thead { tr { th { "Competition / created (UTC)" } th { "Progress" } th { "Inspect" } } }
                        tbody { @for c in candidates.iter().take(50) { tr {
                            td { a href=(format!("/admin/operations/{}",c.id)) { (c.event_submission.locations.join(" · ")) } br; code { (c.id) } br; (c.created_at.format(&Rfc3339).unwrap_or_default()) }
                            td { (c.get_state()) br; (c.total_signed_entries) " signed / " (c.total_entries) " entries" }
                            td { a href=(format!("/admin/operations/{}",c.id)) { "Milestones & errors" } br; a href=(format!("/admin/funds?competition={}",c.id)) { "Entry & fund traces" } }
                        } } }
                    } }
                }
            }
            Err(_) => p.notice { "Competition records are unavailable. Monitoring cannot determine which entries are affected." },
        }
    }};
    render_admin_fragment(&headers, &state, &csrf, "Keymeld & enclaves", content)
}

fn lookup(value: &str) -> Markup {
    html! {
        form.discovery-filters method="get" action="/admin/funds" { label { "Entry, ticket, competition, payment hash, or swap" input name="q" value=(value) maxlength="256" required; } button type="submit" { "Find customer funds" } }
    }
}

fn keymeld_path(filter: &str) -> String {
    let mut url = reqwest::Url::parse("http://localhost/admin/keymeld").expect("static URL");
    url.query_pairs_mut().append_pair("q", filter);
    format!("{}?{}", url.path(), url.query().unwrap_or_default())
}

fn signals(
    state: &AppState,
    panel: Panel,
    data: &Cached<Snapshot>,
    path: &str,
    capabilities: Option<&Cached<crate::infra::keymeld::PayoutCapabilities>>,
) -> Markup {
    signal_panel(
        panel,
        data,
        path,
        state.admin_monitoring.grafana_link(panel == Panel::Keymeld),
        capabilities,
    )
}

fn signal_panel(
    panel: Panel,
    data: &Cached<Snapshot>,
    path: &str,
    grafana: Option<String>,
    capabilities: Option<&Cached<crate::infra::keymeld::PayoutCapabilities>>,
) -> Markup {
    let now = OffsetDateTime::now_utc().unix_timestamp() as f64;
    let cache_fresh = data
        .latest
        .as_ref()
        .is_some_and(|v| v.age() <= Duration::from_secs(120));
    html! { section id="service-signals"
        hx-get=(path) hx-trigger="every 60s" hx-select="#service-signals"
        hx-target="this" hx-swap="outerHTML" hx-sync="#service-signals:drop" hx-push-url="false" {
        @if let Some(capabilities) = capabilities { (verifier(capabilities)) }
        div.signal-heading {
            h2 { "Service signals" }
            div.signal-actions {
                a href=(path) hx-get=(path) hx-target="#service-signals" hx-select="#service-signals" hx-swap="outerHTML" hx-sync="#service-signals:drop" hx-push-url="false" { "Refresh values" }
                @if let Some(link) = grafana { a href=(link) rel="noreferrer" { "Grafana history" } }
            }
        }
        @if let Some(latest) = &data.latest {
            p.note { "Grafana checked " time datetime=(latest.fetched_at.format(&Rfc3339).unwrap_or_default()) { (latest.fetched_at.format(&Rfc3339).unwrap_or_default()) } ". Updates every minute while this page is open." }
            @if !cache_fresh { p.notice { "Grafana has not returned fresh data. Values are unknown until the connection recovers." } }
        } @else {
            p.notice { @if data.refreshing { "Grafana is taking longer than expected. The next refresh will check again." } @else { "Grafana data is unavailable or not configured. Entry records and direct wallet checks remain available." } }
        }
        div.service-signals {
            @for (index, spec) in panel.signals().iter().enumerate() {
                @let samples = data.value().and_then(|d| d.get(index)).and_then(|v| v.as_ref());
                @let known = cache_fresh && samples.is_some_and(|s| !s.is_empty() && s.iter().all(|v| v.fresh_number(now).is_some()));
                @let review = known && samples.is_some_and(|s| s.iter().filter_map(|v|v.fresh_number(now)).any(spec.review));
                article.service-signal {
                    div.signal-heading {
                        h3 { (spec.title) }
                        span class=(if !known { "signal-status" } else if review { "signal-status signal-review" } else { "signal-status signal-ok" }) { @if !known { "Unknown" } @else if review { "Review" } @else { "OK" } }
                    }
                    dl.signal-values {
                        @if let Some(samples) = samples.filter(|s| !s.is_empty()) {
                            @for sample in samples {
                                div {
                                    dt { (sample_label(sample)) }
                                    dd { @match sample.fresh_number(now).filter(|_|cache_fresh) {
                                        Some(value) => (signal_value(spec, value)),
                                        None => "Unknown / stale",
                                    } }
                                }
                            }
                        } @else { div { dt { "Current value" } dd { "Unknown" } } }
                    }
                    details id=(format!("signal-help-{panel:?}-{index}")) hx-preserve {
                        summary { "Investigate" }
                        p { (spec.help) }
                        p { a href=(spec.link) { "Inspect related records" } }
                    }
                }
            }
        }
        @if panel == Panel::Keymeld { (enclaves(data)) }
        noscript { p.note { "Use Refresh values to check again. Automatic updates require JavaScript." } }
    } }
}

fn verifier(capabilities: &Cached<crate::infra::keymeld::PayoutCapabilities>) -> Markup {
    html! { section { h2 { "Coordinator verifier" }
        p { code { (coordinator_escrow::generic::VERIFIER_ID) } " · protocol " (coordinator_escrow::generic::VERIFIER_VERSION) }
        @if let Some(value) = &capabilities.latest {
            p.note { "Checked across advertised enclaves using the confidential protocol at " (value.fetched_at.format(&Rfc3339).unwrap_or_default()) "." }
            @if value.age() > Duration::from_secs(120) { p.notice { "Capability check is stale. Treat support as unknown until a fresh check succeeds." } }
            @else { p { "Payout authorization: " strong { (if value.value.payout { "Supported" } else { "Unavailable" }) } " · Lightning Address resolution: " strong { (if value.value.lnurl { "Supported" } else { "Unavailable" }) } } }
        } @else { p.notice { @if capabilities.refreshing { "Checking verifier capabilities. Reload shortly." } @else { "Verifier capabilities are unknown. Check Keymeld connectivity and the enclave configuration." } } }
        p.note { "Capability support describes configuration. It does not prove a particular entry is authorized, paid, or signed." }
    } }
}

fn signal_value(spec: &Signal, value: f64) -> String {
    let boolean = match spec.title {
        "Service probes" | "Gateway scrape" => Some((1.0, "Reachable", "Unreachable")),
        "Enclave health" => Some((1.0, "Healthy", "Unhealthy")),
        "Stopped coordinator workers" => Some((0.0, "Running", "Stopped")),
        "Lightning and Ark subscriptions" => Some((1.0, "Connected", "Unconfirmed")),
        "Database replication failures" => Some((0.0, "Replicating", "Failed")),
        _ => None,
    };
    if let Some((healthy, yes, no)) = boolean {
        return if value == healthy { yes } else { no }.into();
    }
    if spec.unit == " s" {
        return duration_label(value);
    }
    if value.fract().abs() < 0.05 {
        format!("{value:.0}{}", spec.unit)
    } else {
        format!("{value:.1}{}", spec.unit)
    }
}

fn duration_label(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

fn sample_label(sample: &Sample) -> String {
    match sample.metric.get("__name__").map(String::as_str) {
        Some("coordinator_ln_invoice_subscription_up") => return "Lightning invoices".into(),
        Some("coordinator_ln_payment_subscription_up") => return "Lightning payments".into(),
        Some("coordinator_escrow_subscription_up") => return "Ark escrow updates".into(),
        _ => {}
    }
    if let Some(id) = sample.metric.get("enclave_id") {
        return format!("Enclave {id}");
    }
    let labels: Vec<_> = ["thread", "node", "app", "database", "service"]
        .iter()
        .filter_map(|key| sample.metric.get(*key).map(|value| value.replace('_', " ")))
        .collect();
    if labels.is_empty() {
        "Current value".into()
    } else {
        labels.join(" · ")
    }
}
fn enclaves(data: &Cached<Snapshot>) -> Markup {
    let now = OffsetDateTime::now_utc().unix_timestamp() as f64;
    let fresh = data
        .latest
        .as_ref()
        .is_some_and(|v| v.age() <= Duration::from_secs(120));
    let mut rows: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    if let Some(Some(samples)) = data.value().and_then(|d| d.get(KEYMELD.len())) {
        for sample in samples {
            if let Some(id) = sample.metric.get("enclave_id") {
                rows.entry(id).or_default().push(sample);
            }
        }
    }
    html! { section { h2 { "Enclave observations" }
        @if rows.is_empty() { p.note { "No enclave observations available. Check the Keymeld scrape and gateway version." } }
        div.metric-grid { @for (id, samples) in rows {
            @let observed = samples.iter().find(|s| s.metric.get("field").is_some_and(|f| f == "observed_at")).and_then(|s| s.fresh_number(now));
            @let observed_fresh = observed.is_some_and(|at| now - at <= 120.0 && at <= now + 30.0);
            article.metric { h3 { "Enclave " (id) }
                @for sample in &samples {
                    @if sample.metric.get("__name__").is_some_and(|n| n == "keymeld_enclave_deployment_info") {
                        p { (sample.metric.get("component").map(String::as_str).unwrap_or("Unknown build")) " · " (sample.metric.get("version").map(String::as_str).unwrap_or("Unknown version")) }
                        p.note { "Declared by deployment; not attested build identity." }
                    }
                }
                dl.signal-values {
                    @for sample in &samples {
                        @if let Some((label, value)) = enclave_value(sample, now) {
                            div { dt { (label) } dd {
                                @if fresh && (!sample.metric.contains_key("field") || observed_fresh) { (value) }
                                @else { "Unknown / stale" }
                            } }
                        }
                    }
                }
            }
        } }
    } }
}

fn enclave_value(sample: &Sample, now: f64) -> Option<(&'static str, String)> {
    let name = sample
        .metric
        .get("field")
        .or_else(|| sample.metric.get("__name__"))?
        .as_str();
    let label = match name {
        "active_sessions" => "Active sessions",
        "key_epoch" => "Key epoch",
        "observed_at" => "Last observation",
        "uptime_seconds" => "Uptime",
        "keymeld_enclave_connection_active" => "Gateway connections",
        "keymeld_enclave_connection_avg_load" => "Connection load",
        "keymeld_enclave_failure_rate_percent" => "Request failures",
        _ => return None,
    };
    let value = sample
        .fresh_number(now)
        .map(|value| match name {
            "observed_at" => format!("{} ago", duration_label(now - value)),
            "uptime_seconds" => duration_label(value),
            "keymeld_enclave_failure_rate_percent" => format!("{value:.1}%"),
            "keymeld_enclave_connection_avg_load" => format!("{value:.1}"),
            _ => format!("{value:.0}"),
        })
        .unwrap_or_else(|| "Unknown / stale".into());
    Some((label, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::{admin_signals::SERVICES, refresh_cache::Fetched};

    fn sample(name: &str, value: &str, at: f64) -> Sample {
        Sample {
            metric: BTreeMap::from([
                ("__name__".into(), name.into()),
                ("app".into(), "coordinator".into()),
            ]),
            value: (at, value.into()),
        }
    }

    #[test]
    fn subscription_sources_are_distinguishable_and_values_are_readable() {
        let invoice = sample("coordinator_ln_invoice_subscription_up", "0", 100.0);
        let payment = sample("coordinator_ln_payment_subscription_up", "1", 100.0);
        let ark = sample("coordinator_escrow_subscription_up", "0", 100.0);
        assert_eq!(sample_label(&invoice), "Lightning invoices");
        assert_eq!(sample_label(&payment), "Lightning payments");
        assert_eq!(sample_label(&ark), "Ark escrow updates");
        assert_eq!(signal_value(&SERVICES[5], 0.0), "Unconfirmed");
        assert_eq!(signal_value(&SERVICES[5], 1.0), "Connected");
        assert_eq!(signal_value(&SERVICES[3], 125.0), "2m 5s");
        assert_eq!(signal_value(&SERVICES[10], 45000.0), "12h 30m");
    }

    #[test]
    fn first_render_includes_values_without_queries_or_javascript() {
        let now = OffsetDateTime::now_utc().unix_timestamp() as f64;
        let data = Cached {
            latest: Some(Arc::new(Fetched::new(vec![Some(vec![sample(
                "up", "1", now,
            )])]))),
            refreshing: false,
        };
        let rendered =
            signal_panel(Panel::Keymeld, &data, "/admin/keymeld", None, None).into_string();
        assert!(rendered.contains("Reachable"));
        assert!(rendered.contains("Refresh values"));
        assert!(!rendered.contains("Metric query"));
        for spec in KEYMELD {
            assert!(!rendered.contains(spec.query));
        }
        let stale = Cached {
            latest: Some(Arc::new(Fetched::new(vec![Some(vec![sample(
                "up",
                "1",
                now - 121.0,
            )])]))),
            refreshing: false,
        };
        let rendered =
            signal_panel(Panel::Keymeld, &stale, "/admin/keymeld", None, None).into_string();
        assert!(!rendered.contains("Reachable"));
        assert!(rendered.contains("Unknown / stale"));
    }

    #[test]
    fn refresh_keeps_the_competition_filter_encoded() {
        assert_eq!(
            keymeld_path("KDEN & open"),
            "/admin/keymeld?q=KDEN+%26+open"
        );
    }
}
