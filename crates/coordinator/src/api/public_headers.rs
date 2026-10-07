//! Headers on the public site's pages. The WASM wallet holds the player's key
//! in memory, where injected script cannot read it but could ask it to sign;
//! so the Content-Security-Policy allows no script but this site's own files,
//! nothing inline, no eval, and connections only to the API, oracle and
//! configured signing gateway (and the Satchel wallet, when configured).
//! Trusted Types close the DOM's string-to-code sinks to everything but htmx's
//! own policy (shared/htmx_security.js).

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY,
            X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
        HeaderValue,
    },
    middleware::Next,
    response::Response,
};

/// The policy for the public pages, computed once at startup.
#[derive(Clone)]
pub struct PublicHeaders {
    content_security_policy: HeaderValue,
    recover_policy: HeaderValue,
}

/// The recovery page (`/recover`) holds no wallet signer, and must reach whichever relays and
/// Esplora the player names, so it alone may connect to any https or wss origin.
const RECOVER_PATH: &str = "/recover";

impl PublicHeaders {
    /// `connect` lists the other sites the page's scripts call: the API
    /// (normally this site), oracle and public Keymeld attestation gateway.
    /// `forms` lists the other sites its forms may post to: the Satchel wallet's
    /// sign-in, when configured.
    pub fn new(connect: &[&str], forms: &[&str]) -> Self {
        let policy = content_security_policy(connect, forms);
        Self {
            content_security_policy: HeaderValue::from_str(&policy)
                .expect("origins parsed from URLs are valid header text"),
            recover_policy: HeaderValue::from_static(RECOVER_POLICY),
        }
    }
}

/// The recovery page's policy: the public pages' own, but connections to any https or wss
/// origin, and no Trusted Types policy at all, since its script writes text only.
const RECOVER_POLICY: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self'; img-src 'self' data:; connect-src 'self' https: wss:; object-src 'none'; \
    base-uri 'none'; frame-ancestors 'none'; form-action 'self'; \
    require-trusted-types-for 'script'; trusted-types 'none'";

fn origin(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// - scripts: this site's files only (and compiling the WASM wallet); no
///   inline script, `on*` handlers or eval;
/// - Trusted Types for every script sink, created only by the policy named
///   `htmx`, so HTML and script reach the DOM only through htmx's swaps, and
///   `login-worker`, which admits only the log-in worker's URL (shared/wasm.js);
/// - styles: this site's only (Bulma is vendored); no inline styles;
/// - connections: this site, the API, oracle and configured Keymeld gateway;
/// - forms: this site, and the sites in `forms`.
pub fn content_security_policy(connect: &[&str], forms: &[&str]) -> String {
    let sources = |urls: &[&str]| {
        let mut sources = vec!["'self'".to_owned()];
        for origin in urls.iter().filter_map(|url| origin(url)) {
            if !sources.contains(&origin) {
                sources.push(origin);
            }
        }
        sources.join(" ")
    };
    [
        "default-src 'none'".to_owned(),
        "script-src 'self' 'wasm-unsafe-eval'".to_owned(),
        "style-src 'self'".to_owned(),
        "img-src 'self' data:".to_owned(),
        format!("connect-src {}", sources(connect)),
        "object-src 'none'".to_owned(),
        "base-uri 'none'".to_owned(),
        "frame-ancestors 'none'".to_owned(),
        format!("form-action {}", sources(forms)),
        "require-trusted-types-for 'script'".to_owned(),
        "trusted-types htmx login-worker".to_owned(),
    ]
    .join("; ")
}

/// Adds the policy to every HTML response, and the usual hardening headers
/// to every response. A response to a signed request is the signer's own
/// data, so it is always marked `private, no-store`, whatever its handler set.
pub async fn public_response_headers(
    State(headers): State<Arc<PublicHeaders>>,
    request: Request,
    next: Next,
) -> Response {
    let signed = request.headers().contains_key(AUTHORIZATION);
    let recover = request.uri().path() == RECOVER_PATH;
    let mut response = next.run(request).await;
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let response_headers = response.headers_mut();
    if is_html {
        let policy = if recover {
            &headers.recover_policy
        } else {
            &headers.content_security_policy
        };
        response_headers.insert(CONTENT_SECURITY_POLICY, policy.clone());
        response_headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    if signed {
        response_headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    }
    // A proxy in front of the site (Cloudflare) rewrites HTML it is allowed to
    // transform: it injects its analytics beacon and obfuscates e-mail
    // addresses, and both trip the policy above in htmx swaps. `no-transform`
    // tells it to leave the markup alone.
    if is_html {
        let cache_control = match response_headers
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
        {
            Some(value) if value.contains("no-transform") => None,
            Some(value) => Some(format!("{value}, no-transform")),
            None => Some("no-transform".to_owned()),
        };
        if let Some(value) = cache_control.and_then(|value| HeaderValue::from_str(&value).ok()) {
            response_headers.insert(CACHE_CONTROL, value);
        }
    }
    response_headers
        .entry(X_CONTENT_TYPE_OPTIONS)
        .or_insert(HeaderValue::from_static("nosniff"));
    response_headers
        .entry(REFERRER_POLICY)
        .or_insert(HeaderValue::from_static("strict-origin-when-cross-origin"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_recovery_page_may_reach_any_relay_or_esplora() {
        let headers = PublicHeaders::new(&["https://5day4cast.com"], &[]);
        let recover = headers.recover_policy.to_str().unwrap();
        assert!(recover.contains("connect-src 'self' https: wss:;"));
        assert!(recover.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(recover.contains("trusted-types 'none'"));
        assert!(!recover.contains("unsafe-inline"));
        let public = headers.content_security_policy.to_str().unwrap();
        assert!(!public.contains("https:;") && !public.contains("wss:"));
    }

    #[test]
    fn the_policy_allows_only_this_sites_scripts_and_the_oracle() {
        let policy = content_security_policy(
            &[
                "https://5day4cast.com",
                "https://4casttruth.win/",
                "https://4casttruth.win/oracle",
                "https://keymeld.example.net/enclaves",
                "not a url",
            ],
            &[""],
        );
        assert!(policy.contains("default-src 'none'"));
        assert!(policy.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(!policy.contains("unsafe-inline"));
        assert!(!policy.contains("'unsafe-eval'"));
        assert!(policy.contains("connect-src 'self' https://5day4cast.com https://4casttruth.win https://keymeld.example.net;"));
        assert!(policy.contains("style-src 'self';"));
        for directive in [
            "require-trusted-types-for 'script'",
            "trusted-types htmx login-worker",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "form-action 'self';",
        ] {
            assert!(policy.contains(directive), "{directive}");
        }
    }

    /// Pages post the Satchel sign-in form and look up the player's Satchel address, so its
    /// origin is allowed for both, and only when it is configured.
    #[test]
    fn the_satchel_wallet_is_allowed_only_when_configured() {
        let satchel = "https://wallet.5day4cast.com";
        let policy = content_security_policy(&["https://5day4cast.com", satchel], &[satchel]);
        assert!(policy.contains(&format!(
            "connect-src 'self' https://5day4cast.com {satchel};"
        )));
        assert!(policy.contains(&format!("form-action 'self' {satchel};")));

        let without = content_security_policy(&["https://5day4cast.com", ""], &[""]);
        assert!(without.contains("connect-src 'self' https://5day4cast.com;"));
        assert!(without.contains("form-action 'self';"));
        assert!(!without.contains("wallet"));
    }
}
