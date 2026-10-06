//! A queued competition's kickoff: forming its pools when registration closes.
//!
//! When observation starts, registration is closed. The kickoff takes the first block at or after
//! that time, the lowest height whose header time is at least the close, and the complete
//! tickets: paid, their escrow VTXO funded, an entry submitted and the key deposit stored. The
//! seed over the competition, those tickets and the block's hash splits them into pools
//! (`coordinator_escrow::pools`), so the coordinator cannot choose who plays whom.
//!
//! - Too few for a pool: the queued competition is cancelled, and cleanup refunds every escrow
//!   through a session made for the refunds.
//! - Pools: one write creates each pool's competition, moves its tickets and entries to it,
//!   records the formation, and marks the queued competition as formed. Then each pool gets a
//!   Keymeld session whose members are its players' deposits, each on the enclave it was sealed
//!   to, and runs the usual lifecycle from escrow confirmation. Paid tickets no pool took stay
//!   with the queued competition, and cleanup refunds them.
//!
//! A kickoff that stops after the write resumes without forming pools again: the queued
//! competition is formed, and each pool makes its own session if it has none yet. See
//! `queued.rs` and `docs/QUEUED_COMPETITIONS.md`.

use super::*;
use crate::domain::competitions::{
    queued::{first_block_at_or_after, CompetitionKind},
    queued_store::{NewPool, PoolFormation, QueueSettings},
    Lease, Step, StepError, Wait,
};
use crate::infra::bitcoin::BlockSummary;
use bitcoin::BlockHash;
use coordinator_escrow::{
    pools::{self, Formation},
    queued::DepositEvidence,
};
use keymeld_sdk::prelude::EnclaveId;

/// How often a kickoff looks again for the block that closes registration.
const CLOSE_BLOCK_POLL: time::Duration = time::Duration::seconds(30);
/// Headers read at a time while looking for the closing block: about a day of blocks.
const HEADER_CHUNK: u32 = 144;
/// How long a pool may wait for its Keymeld session and oracle event before it fails, and is
/// refunded.
pub(super) const POOL_SETUP_DEADLINE: time::Duration = time::Duration::hours(1);

/// The first block at or after registration closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseBlock {
    pub height: u32,
    pub hash: BlockHash,
}

impl Coordinator {
    /// One step of a queued competition. Before registration closes there is nothing to do;
    /// at close it forms pools, or is cancelled if too few tickets are complete. After that it
    /// has no lifecycle of its own.
    pub(super) async fn advance_queued_competition(
        &self,
        competition: Competition,
        lease: &Lease,
    ) -> Result<Step, StepError> {
        if competition.is_cancelled() || competition.is_failed() {
            return Ok(Step::Finished);
        }
        if competition.pools_formed_at.is_some() {
            self.start_pools(competition.id).await;
            return Ok(Step::Finished);
        }
        if self
            .cancel_unstarted_for_settle_only(&competition, lease)
            .await
            .map_err(|e| anyhow!("Cannot cancel queued competition: {e:#}"))?
        {
            return Ok(Step::Finished);
        }
        let now = OffsetDateTime::now_utc();
        let close = competition.event_submission.start_observation_date;
        if now < close {
            return Ok(Step::Next(Wait::Until(close)));
        }
        let Some(block) = self
            .close_block(close.unix_timestamp())
            .await
            .map_err(|e| anyhow!("Cannot find the block that closed registration: {e:#}"))?
        else {
            debug!(
                "Queued competition {} waits for the first block after its registration closed",
                competition.id
            );
            return Ok(Step::Next(Wait::Until(now + CLOSE_BLOCK_POLL)));
        };
        let settings = self
            .queue_settings(competition.id)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let tickets = self
            .competition_store
            .complete_queued_tickets(competition.id)
            .await
            .map_err(|e| anyhow!("Cannot read the complete tickets: {e}"))?;
        let formation = pools::form(&settings.pool_rules, competition.id, &tickets, &block.hash)
            .map_err(|e| anyhow!("Cannot form pools: {e}"))?;
        let Formation::Pools { pools, .. } = formation else {
            let cancelled = self
                .competition_store
                .cancel_queued_competition(competition.id, lease)
                .await
                .map_err(|e| anyhow!("Cannot cancel queued competition: {e}"))?;
            if !cancelled {
                return Ok(Step::Next(Wait::Now));
            }
            info!(
                "Queued competition {} closed with {} complete tickets, too few for a pool of \
                 {}; it is cancelled and its escrows will be refunded",
                competition.id,
                tickets.len(),
                settings.pool_rules.min_players()
            );
            return Ok(Step::Finished);
        };
        let formation = pool_formation(&competition, &settings, tickets, block, pools)
            .map_err(|e| anyhow!("Cannot form pools: {e}"))?;
        let count = formation.pools.len();
        let placed = formation.tickets.len();
        let formed = self
            .competition_store
            .form_queued_pools(formation, lease)
            .await
            .map_err(|e| anyhow!("Cannot record the pools: {e}"))?;
        if !formed {
            // Formed, cancelled or taken over meanwhile: the next step reloads it.
            return Ok(Step::Next(Wait::Now));
        }
        info!(
            "Queued competition {} formed {count} pools from {placed} tickets at block {} ({})",
            competition.id, block.height, block.hash
        );
        self.start_pools(competition.id).await;
        Ok(Step::Finished)
    }

    /// Give each pool of a queued competition its Keymeld session, and wake it. A pool that
    /// cannot get one now makes it at its own next step.
    async fn start_pools(&self, parent_id: Uuid) {
        let pools = match self.competition_store.competition_pools(parent_id).await {
            Ok(pools) => pools,
            Err(e) => {
                warn!("Cannot list the pools of queued competition {parent_id}: {e}");
                return;
            }
        };
        for pool in pools {
            match self
                .competition_store
                .get_competition(pool.competition_id)
                .await
            {
                Ok(competition) => {
                    if let Err(e) = self.ensure_pool_session(&competition).await {
                        warn!(
                            "Pool {} has no Keymeld session yet: {e:#}",
                            pool.competition_id
                        );
                    }
                }
                Err(e) => warn!("Cannot load pool {}: {e}", pool.competition_id),
            }
            self.wake_competition(pool.competition_id);
        }
    }

    /// The first block at or after `close`, UNIX seconds, once the chain has one.
    async fn close_block(&self, close: i64) -> Result<Option<CloseBlock>, anyhow::Error> {
        let tip = self.bitcoin.get_current_height().await?;
        let tip_header = self.bitcoin.block_headers(tip, 1).await?;
        if !tip_header
            .first()
            .is_some_and(|block| i64::from(block.time) >= close)
        {
            // Header times may run a little out of order, but a tip before the close means the
            // closing block is still to come.
            return Ok(None);
        }
        // Walk back a chunk at a time until a chunk starts before the close.
        let mut start = tip;
        let mut blocks: Vec<BlockSummary> = Vec::new();
        loop {
            start = start.saturating_sub(HEADER_CHUNK);
            let count = blocks.first().map_or(tip + 1, |first| first.height) - start;
            let mut chunk = self.bitcoin.block_headers(start, count).await?;
            if chunk.len() != count as usize
                || chunk.first().map(|block| block.height) != Some(start)
            {
                return Err(anyhow!(
                    "the node returned an incomplete run of block headers"
                ));
            }
            chunk.append(&mut blocks);
            blocks = chunk;
            if let Some(block) = first_block_at_or_after(&blocks, close) {
                return Ok(Some(CloseBlock {
                    height: block.height,
                    hash: block.hash,
                }));
            }
            if start == 0 {
                // Every block is known: the first at or after the close is the one.
                let block = blocks
                    .iter()
                    .find(|block| i64::from(block.time) >= close)
                    .ok_or_else(|| anyhow!("no block at or after the close"))?;
                return Ok(Some(CloseBlock {
                    height: block.height,
                    hash: block.hash,
                }));
            }
        }
    }

    /// Make a pool's Keymeld session if it has none: the coordinator and the pool's members,
    /// each on the enclave their deposit was sealed to, with the deposit scope of the pool's
    /// queued competition and evidence of how the pool was formed. Its subsets are those of a
    /// pool of its size, members in ticket order.
    pub(super) async fn ensure_pool_session(
        &self,
        competition: &Competition,
    ) -> Result<(), anyhow::Error> {
        if self
            .competition_store
            .get_keymeld_session(competition.id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let (settings, record) = self
            .pool_of(competition)
            .await?
            .ok_or_else(|| anyhow!("Competition {} is not a pool", competition.id))?;
        let entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?;
        let mut members = Vec::with_capacity(record.members.len());
        for ticket in &record.members {
            let context = entries
                .iter()
                .find(|entry| entry.ticket_id == *ticket)
                .and_then(|entry| entry.keymeld_registration_context.as_ref())
                .ok_or_else(|| anyhow!("Pool member {ticket} has no entry with a key deposit"))?;
            members.push((UserId::from(*ticket), context.enclave_id));
        }
        let evidence = DepositEvidence::Pool {
            competition_id: record.parent_id,
            tickets: record.tickets.clone(),
            block_hash: record.block_hash,
            pool_index: record.pool_index as usize,
        };
        // The enclave recomputes the pool from the seed; so does the coordinator, first.
        if evidence.pool_members(&settings.pool_rules)? != Some(record.members.clone()) {
            return Err(anyhow!(
                "Pool {}'s members do not follow from its seed",
                competition.id
            ));
        }
        let scope = Self::deposit_scope_request(&settings, &evidence)?;
        let players: Vec<UserId> = record.members.iter().copied().map(UserId::from).collect();
        let subsets = compute_dlc_subset_definitions(
            self.keymeld.coordinator_user_id(),
            &players,
            settings.terms.number_of_places_win as usize,
        );
        let session = self
            .keymeld
            .init_deposit_session(competition.id, scope, members, subsets)
            .await
            .map_err(|e| anyhow!("Keymeld pool session: {e}"))?;
        self.store_keymeld_session(competition.id, session).await?;
        info!(
            "Pool {} of queued competition {} has its Keymeld session",
            competition.id, record.parent_id
        );
        Ok(())
    }

    /// For a queued competition, or a pool that has no session, a session made only to refund
    /// `members`' escrows: their deposits, each on the enclave it was sealed to, under the deposit
    /// scope with refund evidence, which lets nothing be bound. It is stored as the
    /// competition's session, so the usual refunds sign through it. `None` for any other
    /// competition.
    pub(crate) async fn make_queued_refund_session(
        &self,
        competition_id: Uuid,
        members: Vec<(Uuid, EnclaveId)>,
    ) -> Result<Option<()>, Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;
        let queue = match competition.kind {
            CompetitionKind::Queued => competition.id,
            CompetitionKind::Pool => competition
                .parent_id
                .ok_or_else(|| anyhow!("Pool {competition_id} has no queued competition"))?,
            CompetitionKind::Single => return Ok(None),
        };
        if members.is_empty() {
            return Ok(None);
        }
        let settings = self.queue_settings(queue).await?;
        let scope = Self::deposit_scope_request(
            &settings,
            &DepositEvidence::Refund {
                competition_id: queue,
            },
        )?;
        let session = self
            .keymeld
            .init_deposit_session(
                Uuid::now_v7(),
                scope,
                members
                    .into_iter()
                    .map(|(ticket, enclave)| (UserId::from(ticket), enclave))
                    .collect(),
                DlcSubsetInfo {
                    definitions: vec![],
                    outcome_subset_ids: BTreeMap::new(),
                },
            )
            .await
            .map_err(|e| anyhow!("Keymeld refund session: {e}"))?;
        self.store_keymeld_session(competition_id, session).await?;
        info!("Competition {competition_id} has a Keymeld session for its refunds");
        Ok(Some(()))
    }
}

/// The pools to create: each gets a new id, its members sorted, and its oracle event, which is
/// the queued competition's reference event with the pool's id, size and funding value.
fn pool_formation(
    competition: &Competition,
    settings: &QueueSettings,
    mut tickets: Vec<Uuid>,
    block: CloseBlock,
    pools: Vec<Vec<Uuid>>,
) -> Result<PoolFormation, String> {
    tickets.sort_unstable();
    let pools = pools
        .into_iter()
        .enumerate()
        .map(|(index, mut members)| {
            members.sort_unstable();
            let pool_id = Uuid::now_v7();
            Ok(NewPool {
                competition_id: pool_id,
                pool_index: u32::try_from(index).map_err(|_| "too many pools")?,
                event_submission: super::super::queued::pool_event(
                    &competition.event_submission,
                    pool_id,
                    members.len(),
                    settings.stake_sats,
                )?,
                members,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(PoolFormation {
        parent_id: competition.id,
        close_height: block.height,
        block_hash: block.hash,
        tickets,
        pools,
        formed_at: OffsetDateTime::now_utc(),
    })
}
