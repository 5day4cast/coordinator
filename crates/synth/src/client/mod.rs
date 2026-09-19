pub mod admin;
pub mod auth;
pub mod competitions;
pub mod entries;
pub mod wallet;

use anyhow::{Context, Result};
use reqwest::{Client, RequestBuilder};
use std::{path::Path, sync::Arc};
use zeroize::Zeroizing;

/// HTTP client for the coordinator API
#[derive(Clone)]
pub struct CoordinatorClient {
    http: Client,
    base_url: String,
    admin_url: String,
    /// Operator bearer token for `admin_url`; never logged.
    admin_token: Option<Arc<Zeroizing<String>>>,
}

impl CoordinatorClient {
    pub fn new(base_url: &str, admin_url: Option<&str>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            admin_url: admin_url
                .unwrap_or(base_url)
                .trim_end_matches('/')
                .to_string(),
            admin_token: None,
        }
    }

    /// Read the operator token from `path` (surrounding whitespace ignored).
    pub fn with_admin_token_file(mut self, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read admin token file {}", path.display()))?,
        );
        let token = contents.trim();
        anyhow::ensure!(
            !token.is_empty(),
            "Admin token file {} is empty",
            path.display()
        );
        self.admin_token = Some(Arc::new(Zeroizing::new(token.to_owned())));
        Ok(self)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    /// A POST to the operator listener, carrying the bearer token when configured.
    pub fn admin_post(&self, url: &str) -> RequestBuilder {
        let request = self.http.post(url);
        match &self.admin_token {
            Some(token) => request.bearer_auth(token.as_str()),
            None => request,
        }
    }
}
