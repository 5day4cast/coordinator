//! Read-only operator dependency views. No signing or payment actions live here.
use super::admin::render_admin_fragment;
use crate::{
    api::admin_auth::AdminCsrf,
    infra::{
        admin_signals::{Panel, Sample, Snapshot, KEYMELD},
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
        (signals(&state, Panel::Services, &data))
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
        state.coordinator.list_competitions(),
    );
    let content = html! { main.admin-workspace {
        p.eyebrow { "Registration · signing · escrow" } h1 { "Keymeld & enclaves" }
        (lookup(&filter.q))
        p.note { "The gateway relays confidential traffic. Its session list does not include that work. Use competition records to identify affected entries; transport activity alone cannot prove signing or payout success." }
        section { h2 { "Coordinator verifier" }
            p { code { (coordinator_escrow::generic::VERIFIER_ID) } " · protocol " (coordinator_escrow::generic::VERIFIER_VERSION) }
            @if let Some(value) = &capabilities.latest {
                p.note { "Checked across advertised enclaves using the confidential protocol at " (value.fetched_at.format(&Rfc3339).unwrap_or_default()) "." }
                @if value.age() > Duration::from_secs(120) { p.notice { "Capability check is stale. Treat support as unknown until a fresh check succeeds." } }
                @else { p { "Payout authorization: " strong { (if value.value.payout { "Supported" } else { "Unavailable" }) } " · Lightning Address resolution: " strong { (if value.value.lnurl { "Supported" } else { "Unavailable" }) } } }
            } @else { p.notice { @if capabilities.refreshing { "Checking verifier capabilities. Reload shortly." } @else { "Verifier capabilities are unknown. Check Keymeld connectivity and the enclave configuration." } } }
            p.note { "Capability support describes configuration. It does not prove a particular entry is authorized, paid, or signed." }
        }
        (signals(&state, Panel::Keymeld, &data))
        (enclaves(&data))
        h2 { "Competitions to inspect" }
        p.note { "Newest first. These records have signing in progress or retained signing errors; inclusion does not establish a current Keymeld fault." }
        form.discovery-filters method="get" action="/admin/keymeld" { label { "Competition ID or station" input name="q" value=(&filter.q) maxlength="256"; } button type="submit" { "Filter competitions" } }
        @match competitions {
            Ok(mut competitions) => {
                competitions.sort_by(|a,b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
                let search = filter.q.trim().to_lowercase();
                let candidates: Vec<_> = competitions.iter().filter(|c| {
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

fn signals(state: &AppState, panel: Panel, data: &Cached<Snapshot>) -> Markup {
    let now = OffsetDateTime::now_utc().unix_timestamp() as f64;
    let cache_fresh = data
        .latest
        .as_ref()
        .is_some_and(|v| v.age() <= Duration::from_secs(120));
    html! { section {
        h2 { "Service signals" }
        @if let Some(link) = state.admin_monitoring.grafana_link(panel == Panel::Keymeld) { p { a href=(link) rel="noreferrer" { "Open Grafana history" } } }
        @if let Some(latest) = &data.latest { p.note { "Queried " (latest.fetched_at.format(&Rfc3339).unwrap_or_default()) ". Refresh on demand, at most once per minute." @if !cache_fresh { " Cached results are stale." } } }
        @else { p.notice { @if data.refreshing { "Fetching Grafana data. Reload shortly." } @else { "Grafana data is unavailable or not configured. Entry records and direct wallet checks remain available." } } }
        div.service-signals {
            @for (index, spec) in panel.signals().iter().enumerate() {
                let samples = data.value().and_then(|d| d.get(index)).and_then(|v| v.as_ref());
                let known = cache_fresh && samples.is_some_and(|s| !s.is_empty() && s.iter().all(|v| v.fresh_number(now).is_some()));
                let review = known && samples.is_some_and(|s| s.iter().filter_map(|v|v.fresh_number(now)).any(spec.review));
                details.service-signal {
                    summary { strong { (spec.title) } " · " span class=(if !known { "note" } else if review { "notice" } else { "note" }) { @if !known { "Unknown" } @else if review { "Review" } @else { "Within check range" } } }
                    p { (spec.help) } p { a href=(spec.link) { "Inspect related records" } }
                    @if let Some(samples) = samples { @for sample in samples {
                        p { (sample_label(sample)) " · " @match sample.fresh_number(now).filter(|_|cache_fresh) { Some(value) => { strong { (format!("{value:.1}")) } (spec.unit) }, None => "Unknown / stale", } }
                    } } @else { p.note { "No usable response. Check the data source, permissions, and scrape status." } }
                    details { summary { "Metric query" } code { (spec.query) } }
                }
            }
        }
    } }
}
fn sample_label(sample: &Sample) -> String {
    let labels: Vec<_> = ["app", "enclave_id", "thread", "database", "node", "field"]
        .iter()
        .filter_map(|k| sample.metric.get(*k).map(|v| format!("{k}: {v}")))
        .collect();
    if labels.is_empty() {
        sample
            .metric
            .get("__name__")
            .cloned()
            .unwrap_or_else(|| "Value".into())
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
            let observed = samples.iter().find(|s| s.metric.get("field").is_some_and(|f| f == "observed_at")).and_then(|s| s.fresh_number(now));
            let observed_fresh = observed.is_some_and(|at| now - at <= 120.0 && at <= now + 30.0);
            article.metric { h3 { "Enclave " (id) }
                @for sample in &samples {
                    @if sample.metric.get("__name__").is_some_and(|n| n == "keymeld_enclave_deployment_info") {
                        p { (sample.metric.get("component").map(String::as_str).unwrap_or("Unknown build")) " · " (sample.metric.get("version").map(String::as_str).unwrap_or("Unknown version")) }
                        p.note { "Declared by deployment; not attested build identity." }
                    }
                }
                details { summary { "Health, sessions & connections" }
                    @for sample in &samples {
                        @if !sample.metric.get("__name__").is_some_and(|n|n == "keymeld_enclave_deployment_info") {
                            p { (sample.metric.get("field").or_else(||sample.metric.get("__name__")).map(String::as_str).unwrap_or("Metric")) " · " @match sample.fresh_number(now).filter(|_|fresh && (!sample.metric.contains_key("field") || observed_fresh)) { Some(value) => (format!("{value:.0}")), None => "Unknown / stale", } }
                        }
                    }
                }
            }
        } }
    } }
}
