use super::CoordinatorClient;
use anyhow::{Context, Result};
use uuid::Uuid;

impl CoordinatorClient {
    /// Settle a ticket's HODL invoice for testing purposes.
    /// This endpoint must be enabled on the coordinator (testMode.enabled: true).
    pub async fn test_settle_invoice(&self, ticket_id: &Uuid) -> Result<()> {
        let url = format!(
            "{}/admin/api/test/settle-invoice/{}",
            self.admin_url(),
            ticket_id
        );

        let resp = self
            .http()
            .post(&url)
            .send()
            .await
            .context("Failed to settle invoice")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Test settle invoice failed ({}): {}. Is testMode enabled on coordinator?",
                status,
                body
            );
        }

        Ok(())
    }

    /// Health check
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/health_check", self.base_url());
        let resp = self
            .http()
            .get(&url)
            .send()
            .await
            .context("Failed to health check")?;

        if !resp.status().is_success() {
            anyhow::bail!("Health check failed: {}", resp.status());
        }

        Ok(())
    }
}
