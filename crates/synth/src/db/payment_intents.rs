//! Persist a rebalance's invoice before sending; unresolved outcomes block replacement invoices.
use super::{now_rfc3339, Rebalance, SynthDb};
use crate::payment::ValidatedInvoice;
use anyhow::{Context, Result};

#[derive(sqlx::FromRow)]
pub struct PendingRebalance {
    pub id: String,
    pub invoice_json: String,
}

impl PendingRebalance {
    pub fn invoice(&self) -> Result<ValidatedInvoice> {
        serde_json::from_str(&self.invoice_json).context("decode durable rebalance invoice")
    }
}

impl SynthDb {
    pub(super) async fn migrate_payment_intents(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rebalance_payment_intents (
            id TEXT PRIMARY KEY REFERENCES rebalances(id),
            kind TEXT NOT NULL,
            payment_hash TEXT NOT NULL UNIQUE,
            invoice_json TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending', 'succeeded', 'failed'))
        )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS one_pending_rebalance_per_leg
            ON rebalance_payment_intents(kind) WHERE state = 'pending'",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_rebalance(&self, kind: &str) -> Result<Option<PendingRebalance>> {
        Ok(sqlx::query_as("SELECT id, invoice_json FROM rebalance_payment_intents WHERE kind = ? AND state = 'pending'")
            .bind(kind).fetch_optional(&self.pool).await?)
    }

    pub async fn begin_rebalance_payment(
        &self,
        rebalance: &Rebalance,
        invoice: &ValidatedInvoice,
    ) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO rebalances (id, kind, channel_id, amount_sats, local_before_sats,
            capacity_sats, status, created_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
        )
        .bind(&id)
        .bind(rebalance.kind)
        .bind(&rebalance.channel_id)
        .bind(i64::try_from(rebalance.amount_sats)?)
        .bind(i64::try_from(rebalance.local_before_sats)?)
        .bind(i64::try_from(rebalance.capacity_sats)?)
        .bind(now_rfc3339()?)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO rebalance_payment_intents (id, kind, payment_hash, invoice_json) VALUES (?, ?, ?, ?)")
            .bind(&id).bind(rebalance.kind).bind(invoice.payment_hash()).bind(serde_json::to_string(invoice)?)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn finish_rebalance_payment(&self, id: &str, error: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE rebalance_payment_intents SET state = ? WHERE id = ? AND state = 'pending'",
        )
        .bind(if error.is_some() {
            "failed"
        } else {
            "succeeded"
        })
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE rebalances SET status = ?, error_message = ? WHERE id = ?")
            .bind(if error.is_some() { "failed" } else { "moved" })
            .bind(error)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_rebalance_uncertain(&self, id: &str, reason: &str) -> Result<()> {
        sqlx::query("UPDATE rebalances SET status = 'uncertain', error_message = ? WHERE id = ?")
            .bind(reason)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uncertainty_survives_restart_and_blocks_a_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synth.sqlite");
        let db = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        let invoice: ValidatedInvoice = serde_json::from_value(serde_json::json!({
            "invoice": "recorded invoice", "payment_hash": "ab".repeat(32),
            "amount_sats": 50, "expires_at": 1234
        }))
        .unwrap();
        let rebalance = Rebalance {
            kind: "channel",
            channel_id: "1".into(),
            amount_sats: 50,
            local_before_sats: 0,
            capacity_sats: 100,
            txid: None,
            error: None,
        };
        let id = db
            .begin_rebalance_payment(&rebalance, &invoice)
            .await
            .unwrap();
        db.mark_rebalance_uncertain(&id, "stream disconnected")
            .await
            .unwrap();
        db.pool.close().await;
        let reopened = SynthDb::new(path.to_str().unwrap()).await.unwrap();
        assert_eq!(
            reopened
                .pending_rebalance("channel")
                .await
                .unwrap()
                .unwrap()
                .id,
            id
        );
        assert!(reopened
            .begin_rebalance_payment(&rebalance, &invoice)
            .await
            .is_err());
        assert_eq!(reopened.list_rebalances(10).await.unwrap().len(), 1);
        reopened.finish_rebalance_payment(&id, None).await.unwrap();
        assert!(reopened
            .pending_rebalance("channel")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            reopened.list_rebalances(10).await.unwrap()[0].status,
            "moved"
        );
    }
}
