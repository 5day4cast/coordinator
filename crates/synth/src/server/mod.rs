mod assets;
mod format;
pub mod live;
pub mod metrics;
mod money;
pub mod routes;
pub mod run_detail;
mod stuck;

use crate::config::SynthConfig;
use crate::rebalance::Rebalancer;
use crate::runner::Runner;
use crate::trail::tracker::Tracker;
use axum::http::{header, HeaderValue};
use axum::Router;
use log::info;
use std::net::SocketAddr;

pub async fn start_server(
    config: &SynthConfig,
    runner: Runner,
    rebalancer: Option<Rebalancer>,
    tracker: Tracker,
) -> anyhow::Result<()> {
    let dashboard = routes::Dashboard {
        runner,
        scenario_config: config.scenario_config(),
        rebalancer,
        tracker,
        live: live::Live::new(),
    };
    // Pages are pushed their live part as things change, rendered once however many watch.
    tokio::spawn(live::render_changes(dashboard.clone()));
    let app = Router::new()
        .merge(routes::router(dashboard))
        .merge(assets::router())
        .merge(metrics::router())
        .layer(axum::middleware::map_response(secure));

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    info!("Synth server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// What every page may load and run: its own script and styles, nothing inline and no eval.
/// htmx 4 has no switch to stop it running `hx-on` handlers or `<script>` tags in swapped
/// content, so this policy is what keeps injected markup from running. Trusted Types close the
/// DOM's string-to-HTML and string-to-script sinks to all but the `htmx` policy, which synth's
/// own script gives htmx (assets/security.js).
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
     form-action 'self'; frame-ancestors 'none'; require-trusted-types-for 'script'; \
     trusted-types htmx";

async fn secure(mut response: axum::response::Response) -> axum::response::Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::CONTENT_SECURITY_POLICY;

    /// Nothing inline, no eval, and only htmx's policy may turn strings into HTML or script.
    #[test]
    fn the_policy_runs_only_synths_own_script() {
        let directives: Vec<&str> = CONTENT_SECURITY_POLICY.split("; ").collect();
        assert!(directives.contains(&"script-src 'self'"));
        assert!(directives.contains(&"require-trusted-types-for 'script'"));
        assert!(directives.contains(&"trusted-types htmx"));
        assert!(!CONTENT_SECURITY_POLICY.contains("unsafe"));
    }
}
