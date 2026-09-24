//! Headers on the public site's pages. The WASM wallet holds the player's key
//! in memory, where injected script cannot read it but could ask it to sign;
//! so the Content-Security-Policy allows no script but this site's own files,
//! nothing inline, and connections only to the API, oracle and configured signing gateway.

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

use crate::templates::layouts::base::BULMA_CSS;

/// The policy for the public pages, computed once at startup.
#[derive(Clone)]
pub struct PublicHeaders {
    content_security_policy: HeaderValue,
}

impl PublicHeaders {
    /// `connect` lists the other sites the page's scripts call: the API
    /// (normally this site), oracle and public Keymeld attestation gateway.
    pub fn new(connect: &[&str]) -> Self {
        let policy = content_security_policy(connect);
        Self {
            content_security_policy: HeaderValue::from_str(&policy)
                .expect("origins parsed from URLs are valid header text"),
        }
    }
}

fn origin(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// - scripts: this site's files only (and compiling the WASM wallet); no
///   inline script, `on*` handlers or eval;
/// - styles: this site's and the pinned Bulma file; no inline styles;
/// - connections: this site, the API, oracle and configured Keymeld gateway.
pub fn content_security_policy(connect: &[&str]) -> String {
    let mut connect_src = vec!["'self'".to_owned()];
    for origin in connect.iter().filter_map(|url| origin(url)) {
        if !connect_src.contains(&origin) {
            connect_src.push(origin);
        }
    }
    [
        "default-src 'none'".to_owned(),
        "script-src 'self' 'wasm-unsafe-eval'".to_owned(),
        format!("style-src 'self' {BULMA_CSS}"),
        "img-src 'self' data:".to_owned(),
        format!("connect-src {}", connect_src.join(" ")),
        "object-src 'none'".to_owned(),
        "base-uri 'none'".to_owned(),
        "frame-ancestors 'none'".to_owned(),
        "form-action 'self'".to_owned(),
    ]
    .join("; ")
}

/// Adds the policy to every HTML response, and the usual hardening headers
/// to every response. A response to a signed request is the signer's own
/// data, so unless its handler says otherwise nothing may store it.
pub async fn public_response_headers(
    State(headers): State<Arc<PublicHeaders>>,
    request: Request,
    next: Next,
) -> Response {
    let signed = request.headers().contains_key(AUTHORIZATION);
    let mut response = next.run(request).await;
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let response_headers = response.headers_mut();
    if is_html {
        response_headers.insert(
            CONTENT_SECURITY_POLICY,
            headers.content_security_policy.clone(),
        );
        response_headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    if signed {
        response_headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
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
    fn the_policy_allows_only_this_sites_scripts_and_the_oracle() {
        let policy = content_security_policy(&[
            "https://5day4cast.com",
            "https://4casttruth.win/",
            "https://4casttruth.win/oracle",
            "https://keymeld.example.net/enclaves",
            "not a url",
        ]);
        assert!(policy.contains("default-src 'none'"));
        assert!(policy.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(!policy.contains("unsafe-inline"));
        assert!(!policy.contains("'unsafe-eval'"));
        assert!(policy.contains("connect-src 'self' https://5day4cast.com https://4casttruth.win https://keymeld.example.net;"));
        assert!(policy.contains(
            "style-src 'self' https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css;"
        ));
        for directive in [
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "form-action 'self'",
        ] {
            assert!(policy.contains(directive), "{directive}");
        }
    }
}
