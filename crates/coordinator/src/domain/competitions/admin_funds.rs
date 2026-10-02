//! Read-only support evidence. Explicit projections deliberately exclude keys, preimages and
//! signing packets. A ticket's current hash must match its escrow before the two are joined.
use super::{ArkCommitment, CompetitionStore, Coordinator};
use base64::Engine as _;
use futures::{stream, StreamExt};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Default, FromRow)]
pub struct FundsTicket {
    pub ticket_id: String,
    pub competition_id: String,
    pub entry_id: Option<String>,
    pub entry_pubkey: Option<String>,
    pub payment_hash: String,
    pub invoice: Option<String>,
    pub paid_at: Option<String>,
    pub settled_at: Option<String>,
    pub released_at: Option<String>,
    pub network_fee_sats: i64,
    pub escrow_address: Option<String>,
    pub swap_id: Option<String>,
    pub vtxo: Option<String>,
    pub escrow_sats: Option<i64>,
    pub funded_at: Option<i64>,
    pub sellback_at: Option<String>,
    pub reclaimed_at: Option<String>,
    pub legacy_escrow_tx: Option<String>,
    pub legacy_reclaimed_at: Option<String>,
    pub refund_id: Option<String>,
    pub refund_hash: Option<String>,
    pub refund_invoice: Option<String>,
    pub refund_state: Option<String>,
    pub refund_ark_txid: Option<String>,
    pub refund_fee_sats: Option<i64>,
    pub refund_updated_at: Option<i64>,
    pub refund_error: Option<String>,
    pub write_off: Option<String>,
}
#[derive(Debug, Clone, Default, FromRow)]
pub struct FundsPayout {
    pub id: String,
    pub entry_id: String,
    pub invoice: String,
    pub payment_hash: Option<String>,
    pub amount_sats: i64,
    pub initiated_at: String,
    pub succeeded_at: Option<String>,
    pub failed_at: Option<String>,
    pub send_attempts: i64,
    pub next_send_at: Option<i64>,
    pub lightning_address: Option<String>,
}
#[derive(Debug, Clone, Default, FromRow)]
pub struct FundsJob {
    pub id: String,
    pub payout_id: Option<String>,
    pub attempts: i64,
    pub retry_at: i64,
    pub completed_at: Option<i64>,
    pub failed_at: Option<i64>,
}
#[derive(Debug, Clone, Default)]
pub struct TicketFlow {
    pub ticket: FundsTicket,
    pub payouts: Vec<FundsPayout>,
    pub jobs: Vec<FundsJob>,
}
#[derive(Debug)]
pub struct FundsPage {
    pub total: i64,
    pub tickets: Vec<TicketFlow>,
    pub commitment: Option<ArkCommitment>,
}
#[derive(Debug, FromRow)]
pub struct FundsMatch {
    pub competition_id: String,
    pub ticket_id: Option<String>,
    pub entry_id: Option<String>,
}

const TICKET_SELECT: &str = "SELECT t.id ticket_id,t.event_id competition_id,e.id entry_id,t.hash payment_hash,
 e.ephemeral_pubkey entry_pubkey,t.payment_request invoice,t.paid_at,t.settled_at,t.invoice_cancelled_at released_at,
 CASE WHEN t.network_fee_hash = t.hash THEN t.network_fee_sats ELSE 0 END network_fee_sats,
 a.escrow_address,a.swap_id,a.vtxo_outpoint vtxo,a.vtxo_sats escrow_sats,a.funded_at,
 e.sellback_broadcasted_at sellback_at,e.reclaimed_broadcasted_at reclaimed_at,
 t.escrow_transaction legacy_escrow_tx,t.escrow_reclaimed_at legacy_reclaimed_at,
 r.refund_id,r.payment_hash refund_hash,r.invoice refund_invoice,r.state refund_state,r.ark_txid refund_ark_txid,
 r.fee_sats refund_fee_sats,r.updated_at refund_updated_at,r.error refund_error,w.reason write_off
 FROM tickets t LEFT JOIN entries e ON e.ticket_id=t.id
 LEFT JOIN ticket_ark_escrows a ON a.ticket_id=t.id AND a.ticket_hash=t.hash
 LEFT JOIN ticket_ark_refunds r ON r.ticket_id=a.ticket_id
 LEFT JOIN ticket_ark_refund_write_offs w ON w.ticket_id=a.ticket_id AND w.ticket_hash=t.hash";

impl CompetitionStore {
    pub async fn funds_page(
        &self,
        competition: Uuid,
        ticket: Option<Uuid>,
        offset: u32,
    ) -> Result<FundsPage, sqlx::Error> {
        let condition = " WHERE t.event_id=?1 AND (?2 IS NULL OR t.id=?2) AND (t.reserved_at IS NOT NULL OR t.paid_at IS NOT NULL OR t.payment_request IS NOT NULL OR a.ticket_id IS NOT NULL OR e.id IS NOT NULL)";
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM ({TICKET_SELECT}{condition})"
        ))
        .bind(competition.to_string())
        .bind(ticket.map(|id| id.to_string()))
        .fetch_one(self.db_connection.read())
        .await?;
        let records: Vec<FundsTicket> = sqlx::query_as(&format!(
            "{TICKET_SELECT}{condition} ORDER BY t.id LIMIT 25 OFFSET ?3"
        ))
        .bind(competition.to_string())
        .bind(ticket.map(|id| id.to_string()))
        .bind(offset)
        .fetch_all(self.db_connection.read())
        .await?;
        let flows: Vec<_> = stream::iter(records)
            .map(|ticket| self.ticket_flow(ticket))
            .buffered(4)
            .collect()
            .await;
        Ok(FundsPage {
            total,
            tickets: flows.into_iter().collect::<Result<_, _>>()?,
            commitment: self.ark_commitment(competition).await?,
        })
    }
    pub async fn funds_ticket(&self, ticket: Uuid) -> Result<Option<TicketFlow>, sqlx::Error> {
        let record = sqlx::query_as(&format!("{TICKET_SELECT} WHERE t.id=?"))
            .bind(ticket.to_string())
            .fetch_optional(self.db_connection.read())
            .await?;
        match record {
            Some(record) => self.ticket_flow(record).await.map(Some),
            None => Ok(None),
        }
    }
    async fn ticket_flow(&self, ticket: FundsTicket) -> Result<TicketFlow, sqlx::Error> {
        let mut payouts: Vec<FundsPayout> = sqlx::query_as("SELECT p.id,p.entry_id,p.payout_payment_request invoice,h.payment_hash,p.payout_amount_sats amount_sats,p.initiated_at,p.succeed_at succeeded_at,p.failed_at,p.send_attempts,p.next_send_at,p.lightning_address FROM payouts p LEFT JOIN payout_payment_hashes h ON h.payout_id=p.id WHERE p.entry_id=? ORDER BY p.initiated_at,p.id")
            .bind(&ticket.entry_id).fetch_all(self.db_connection.read()).await?;
        for payout in &mut payouts {
            if payout.payment_hash.is_none() {
                payout.payment_hash = invoice_hash(&payout.invoice);
            }
        }
        let jobs = sqlx::query_as("SELECT id,payout_id,attempts,retry_at,completed_at,failed_at FROM payout_jobs WHERE entry_id=? ORDER BY created_at,id")
            .bind(&ticket.entry_id).fetch_all(self.db_connection.read()).await?;
        Ok(TicketFlow {
            ticket,
            payouts,
            jobs,
        })
    }
    /// Exact identifiers only; never search sensitive JSON signing packets.
    pub async fn find_funds(&self, query: &str) -> Result<Vec<FundsMatch>, sqlx::Error> {
        let mut matches: Vec<FundsMatch> = sqlx::query_as("SELECT DISTINCT t.event_id competition_id,t.id ticket_id,e.id entry_id FROM tickets t
         LEFT JOIN entries e ON e.ticket_id=t.id
         LEFT JOIN ticket_ark_escrows a ON a.ticket_id=t.id AND a.ticket_hash=t.hash
         LEFT JOIN ticket_ark_refunds r ON r.ticket_id=a.ticket_id
         LEFT JOIN payouts p ON p.entry_id=e.id LEFT JOIN payout_payment_hashes h ON h.payout_id=p.id
         LEFT JOIN payout_jobs j ON j.entry_id=e.id
         WHERE t.id=?1 OR e.id=?1 OR t.hash=?1 OR a.swap_id=?1 OR a.vtxo_outpoint=?1 OR substr(a.vtxo_outpoint,1,64)=?1 OR a.escrow_address=?1
           OR r.refund_id=?1 OR r.payment_hash=?1 OR r.ark_txid=?1 OR p.id=?1 OR h.payment_hash=?1 OR j.id=?1
         UNION SELECT id,NULL,NULL FROM competitions WHERE id=?1 OR substr(json_extract(funding_outpoint,'$'),1,64)=?1 OR json_extract(funding_outpoint,'$')=?1
         UNION SELECT event_id,NULL,NULL FROM ark_funded_competitions WHERE batch_id=?1 LIMIT 101")
            .bind(query).fetch_all(self.db_connection.read()).await?;
        // Transaction IDs are derived from the stored transactions, not their JSON text.
        if query.len() == 64 && query.bytes().all(|b| b.is_ascii_hexdigit()) {
            let txid: bitcoin::Txid = query
                .parse()
                .map_err(|_| sqlx::Error::Protocol("Invalid transaction ID".into()))?;
            let scan = async {
                let mut rows = sqlx::query_as::<_,(String,Vec<u8>)>("SELECT id,outcome_transaction FROM competitions WHERE outcome_transaction IS NOT NULL").fetch(self.db_connection.read());
                while let Some(row) = rows.next().await {
                    let (id, bytes) = row?;
                    let tx: bitcoin::Transaction = serde_json::from_slice(&bytes)
                        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                    if tx.compute_txid() == txid
                        && !matches
                            .iter()
                            .any(|m| m.competition_id == id && m.ticket_id.is_none())
                    {
                        matches.push(FundsMatch {
                            competition_id: id,
                            ticket_id: None,
                            entry_id: None,
                        });
                    }
                    if matches.len() > 100 {
                        break;
                    }
                }
                Ok::<_, sqlx::Error>(())
            };
            tokio::time::timeout(std::time::Duration::from_secs(3), scan)
                .await
                .map_err(|_| {
                    sqlx::Error::Protocol(
                        "Transaction lookup deadline reached; search by competition or ticket"
                            .into(),
                    )
                })??;
        }
        Ok(matches)
    }
}

fn same_hash(expected: &str, actual: &str) -> bool {
    let decode = |value: &str| {
        hex::decode(value)
            .ok()
            .filter(|v| v.len() == 32)
            .or_else(|| {
                base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .ok()
                    .filter(|v| v.len() == 32)
            })
    };
    matches!((decode(expected),decode(actual)), (Some(a),Some(b)) if a==b)
}

pub fn invoice_sats(invoice: &str) -> Option<u64> {
    invoice
        .parse::<lightning_invoice::Bolt11Invoice>()
        .ok()?
        .amount_milli_satoshis()
        .filter(|amount| amount % 1000 == 0)
        .map(|amount| amount / 1000)
}
pub fn invoice_hash(invoice: &str) -> Option<String> {
    invoice
        .parse::<lightning_invoice::Bolt11Invoice>()
        .ok()
        .map(|invoice| invoice.payment_hash().to_string())
}

/// Only non-secret facts cross the service boundary. Each service has its own deadline and
/// each answer is kept even if another service fails; this endpoint never sends payments.
#[derive(Debug, Clone)]
pub struct FundsFact {
    pub source: String,
    pub text: String,
}
async fn observe<T, E>(future: impl std::future::Future<Output = Result<T, E>>) -> Option<T> {
    tokio::time::timeout(std::time::Duration::from_secs(3), future)
        .await
        .ok()?
        .ok()
}
impl Coordinator {
    pub async fn inspect_funds(&self, flow: &TicketFlow) -> Vec<FundsFact> {
        let ticket = &flow.ticket;
        let mut facts = Vec::new();
        let fact = |source: &str, text: String| FundsFact {
            source: source.into(),
            text,
        };
        if let (Some(ark), Some(id)) = (
            self.ark(),
            ticket.swap_id.as_deref().and_then(|id| id.parse().ok()),
        ) {
            let (swap, vtxos) = tokio::join!(
                observe(ark.swaps.swap(id)),
                observe(
                    ark.transport
                        .vtxos(ticket.escrow_address.clone().into_iter().collect())
                )
            );
            match swap.as_ref() {
                Some(s) if s.id == id && same_hash(&ticket.payment_hash, &s.payment_hash) && Some(&s.escrow_address)==ticket.escrow_address.as_ref() => {
                    let state=serde_json::to_string(&s.state).unwrap_or_default();
                    facts.push(fact("ark-swapd",format!("{state}; {} sats to escrow; Ark transaction: {}; escrow output: {}. {}",s.amount_sat,s.ark_txid.as_deref().unwrap_or("unknown"),s.escrow_vtxo.as_deref().unwrap_or("unknown"),if s.state.player_paid(){"Swap service reports the customer's Lightning invoice settled."}else if s.state.ended_unpaid(){"Swap service reports the customer's Lightning payment was not taken. Any funded escrow needs service-fund recovery."}else{"Escrow funding alone does not prove the customer's Lightning payment settled."})));
                },
                Some(_) => facts.push(fact("ark-swapd","Identity mismatch: returned swap does not match this ticket's payment hash and escrow. Do not attribute its funds to this customer.".into())),
                None => facts.push(fact("ark-swapd","Lookup unavailable; current swap state unknown.".into())),
            }
            match vtxos {
                Some(vtxos) => {
                    let matches: Vec<_> = vtxos
                        .iter()
                        .filter(|v| {
                            ticket
                                .vtxo
                                .as_ref()
                                .is_some_and(|id| *id == v.outpoint.to_string())
                                || swap
                                    .as_ref()
                                    .filter(|s| {
                                        s.id == id
                                            && same_hash(&ticket.payment_hash, &s.payment_hash)
                                            && Some(&s.escrow_address)
                                                == ticket.escrow_address.as_ref()
                                    })
                                    .and_then(|s| s.escrow_vtxo.as_ref())
                                    .is_some_and(|id| *id == v.outpoint.to_string())
                        })
                        .collect();
                    if matches.is_empty() {
                        facts.push(fact("Arkade indexer","No matching escrow outpoint in the response. This is not proof that funds are gone.".into()));
                    }
                    for v in matches {
                        facts.push(fact("Arkade indexer",format!("{} · {} sats · spent={} · swept={} · unrolled={} · expires at {} · checkpoint/forfeit={} · spending Ark tx={} · settled into batch={}",v.outpoint,v.amount.to_sat(),v.is_spent,v.is_swept,v.is_unrolled,v.expires_at,v.spent_by.map(|v|v.to_string()).unwrap_or_else(||"unknown".into()),v.ark_txid.map(|v|v.to_string()).unwrap_or_else(||"unknown".into()),v.settled_by.map(|v|v.to_string()).unwrap_or_else(||"unknown".into()))));
                    }
                }
                None => facts.push(fact(
                    "Arkade indexer",
                    "Lookup unavailable; current escrow spend status unknown.".into(),
                )),
            }
        } else if ticket.escrow_address.is_some() {
            facts.push(fact(
                "Arkade",
                "Swap service not configured or no swap ID recorded. Current status unknown."
                    .into(),
            ));
        } else {
            match observe(self.ln.lookup_invoice(&ticket.payment_hash)).await {
                Some(invoice) if same_hash(&ticket.payment_hash, &invoice.r_hash) => {
                    facts.push(fact(
                        "Coordinator Lightning",
                        format!(
                            "Incoming invoice {:?}; face value {} sats.",
                            invoice.state, invoice.value
                        ),
                    ))
                }
                _ => facts.push(fact(
                    "Coordinator Lightning",
                    "Incoming invoice lookup unavailable or mismatched; status unknown.".into(),
                )),
            }
        }
        let hashes: std::collections::BTreeSet<_> = flow
            .payouts
            .iter()
            .filter_map(|p| p.payment_hash.clone())
            .chain(ticket.refund_hash.clone())
            .collect();
        if hashes.len() > 50 {
            facts.push(fact("Lookup limit", format!("{} additional payment hashes were not queried. All attempts remain in the trace.", hashes.len() - 50)));
        }
        let more: Vec<_>=stream::iter(hashes.into_iter().take(50)).map(|hash| async move {
            match observe(self.ln.lookup_payment(&hash)).await {
                Some(p) if same_hash(&hash, &p.payment_hash)=>fact("Coordinator Lightning",format!("Outgoing payment {hash} · {:?} · {} sats · routing fee {} sats · failure reason: {}. {}",p.status,p.value_sat,p.fee_sat,p.failure_reason,if p.status==crate::domain::PaymentStatus::Succeeded {"Sender reports success; recipient-node receipt is not independently checked here."}else{"Payment is not confirmed successful by this lookup."})),
                _=>fact("Coordinator Lightning",format!("Outgoing payment {hash}: lookup unavailable or mismatched; current state unknown.")),
            }
        }).buffered(8).collect().await;
        facts.extend(more);
        if let (Some(ark), Some(id)) = (
            self.ark(),
            ticket.refund_id.as_deref().and_then(|id| id.parse().ok()),
        ) {
            match observe(ark.swaps.refund(id)).await {
                Some(r)
                    if r.id == id
                        && ticket
                            .refund_hash
                            .as_ref()
                            .is_some_and(|hash| same_hash(hash, &r.payment_hash)) =>
                {
                    facts.push(fact(
                        "Refund swap",
                        format!(
                            "{:?} · {} sats · output {} · claim transaction {}",
                            r.state,
                            r.amount_sat,
                            r.swap_vtxo.as_deref().unwrap_or("unknown"),
                            r.claim_txid.as_deref().unwrap_or("unknown")
                        ),
                    ))
                }
                _ => facts.push(fact(
                    "Refund swap",
                    "Lookup unavailable or mismatched; current state unknown.".into(),
                )),
            }
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
    #[test]
    fn lightning_hash_matching_accepts_lnd_base64_and_rejects_other_payments() {
        let hash = "ab".repeat(32);
        assert!(same_hash(
            &hash,
            &base64::engine::general_purpose::STANDARD.encode([0xab; 32])
        ));
        assert!(same_hash(&hash, &hash.to_uppercase()));
        assert!(!same_hash(&hash, &"cd".repeat(32)));
        assert!(!same_hash("bad", "bad"));
    }
    #[tokio::test]
    async fn funding_and_outcome_transaction_ids_resolve_to_their_competition() {
        let dir = tempfile::tempdir().unwrap();
        let db = DBConnection::new(
            dir.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let store = CompetitionStore::new(db.clone());
        let id = Uuid::now_v7();
        let tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let txid = tx.compute_txid();
        let funding: bitcoin::OutPoint = format!("{}:3", "a".repeat(64)).parse().unwrap();
        db.execute_write(move|pool|async move {
            sqlx::query("INSERT INTO competitions(id,created_at,event_submission,funding_outpoint,outcome_transaction) VALUES (?,datetime('now'),'{}',?,?)")
                .bind(id.to_string()).bind(serde_json::to_vec(&funding).unwrap()).bind(serde_json::to_vec(&tx).unwrap()).execute(&pool).await?;Ok(())
        }).await.unwrap();
        assert_eq!(
            store.find_funds(&funding.txid.to_string()).await.unwrap()[0].competition_id,
            id.to_string()
        );
        assert_eq!(
            store.find_funds(&funding.to_string()).await.unwrap()[0].competition_id,
            id.to_string()
        );
        assert_eq!(
            store.find_funds(&txid.to_string()).await.unwrap()[0].competition_id,
            id.to_string()
        );
    }
    #[tokio::test]
    async fn trace_keeps_attempts_and_abandoned_tickets_and_does_not_join_recycled_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let db = DBConnection::new(
            dir.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let store = CompetitionStore::new(db.clone());
        let competition = Uuid::now_v7();
        let child = Uuid::now_v7();
        let ticket = Uuid::now_v7();
        let abandoned = Uuid::now_v7();
        let entry = Uuid::now_v7();
        let swap = Uuid::now_v7();
        db.execute_write(move |pool|async move {
            for id in [competition,child] {sqlx::query("INSERT INTO competitions(id,created_at,event_submission) VALUES (?,datetime('now'),'{}')").bind(id.to_string()).execute(&pool).await?;}
            for id in [ticket,abandoned] { sqlx::query("INSERT INTO tickets(id,event_id,encrypted_preimage,hash,reserved_at,payment_request) VALUES (?,?,'SECRET-PREIMAGE','current-hash',datetime('now'),'invoice')").bind(id.to_string()).bind(competition.to_string()).execute(&pool).await?; }
            sqlx::query("INSERT INTO entries(id,event_id,ticket_id,pubkey,ephemeral_pubkey,payout_hash,entry_submission) VALUES (?,?,?,'owner','entry-key','payout-hash','{}')").bind(entry.to_string()).bind(competition.to_string()).bind(ticket.to_string()).execute(&pool).await?;
            sqlx::query("INSERT INTO ticket_ark_escrows(ticket_id,ticket_hash,escrow_tap_tree,escrow_address,swap_id,vtxo_outpoint,vtxo_sats) VALUES (?,'old-hash','PRIVATE-TREE','old-address',?,'old-outpoint',900)").bind(ticket.to_string()).bind(swap.to_string()).execute(&pool).await?;
            for (i,succeeded) in [(0,false),(1,true)] {
                let id=Uuid::now_v7();
                sqlx::query("INSERT INTO payouts(id,entry_id,payout_payment_request,payout_amount_sats,initiated_at,succeed_at,failed_at,payment_preimage) VALUES (?,?,'invoice',5000,datetime('now'),?,?, 'SECRET-PAYOUT-PREIMAGE')")
                    .bind(id.to_string()).bind(entry.to_string()).bind(succeeded.then_some("2026-10-02 01:00:00")).bind((!succeeded).then_some("2026-10-02 00:00:00")).execute(&pool).await?;
                sqlx::query("INSERT INTO payout_payment_hashes(payment_hash,payout_id) VALUES (?,?)").bind(format!("outgoing-{i}")).bind(id.to_string()).execute(&pool).await?;
            }
            Ok(())
        }).await.unwrap();
        let page = store.funds_page(competition, None, 0).await.unwrap();
        assert_eq!(page.total, 2);
        let flow = page
            .tickets
            .iter()
            .find(|t| t.ticket.ticket_id == ticket.to_string())
            .unwrap();
        assert_eq!(flow.payouts.len(), 2);
        assert!(flow.ticket.swap_id.is_none());
        assert_eq!(
            flow.payouts
                .iter()
                .filter(|p| p.succeeded_at.is_some())
                .count(),
            1
        );
        assert!(!format!("{page:?}").contains("SECRET"));
        assert!(!format!("{page:?}").contains("PRIVATE-TREE"));
        assert_eq!(
            store.find_funds("outgoing-1").await.unwrap()[0].entry_id,
            Some(entry.to_string())
        );
        assert!(store.find_funds("' OR 1=1 --").await.unwrap().is_empty());
        db.execute_write(move |pool| async move {
            sqlx::query("UPDATE tickets SET event_id=? WHERE id=?")
                .bind(child.to_string())
                .bind(ticket.to_string())
                .execute(&pool)
                .await?;
            sqlx::query("UPDATE entries SET event_id=? WHERE id=?")
                .bind(child.to_string())
                .bind(entry.to_string())
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            store.funds_page(competition, None, 0).await.unwrap().total,
            1
        );
        assert_eq!(store.funds_page(child, None, 0).await.unwrap().total, 1);
        assert_eq!(
            store.find_funds("outgoing-1").await.unwrap()[0].competition_id,
            child.to_string()
        );
        assert!(store
            .funds_page(child, None, 25)
            .await
            .unwrap()
            .tickets
            .is_empty());
    }
}
