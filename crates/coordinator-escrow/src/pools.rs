//! Splitting a queued competition's tickets into even pools.
//!
//! A queued competition sells entries without a seat count. When registration closes, its complete
//! tickets are split into as few pools as the largest pool size allows, as evenly as possible.
//! Each pool is its own contract, oracle event, and Keymeld session.
//!
//! The coordinator cannot choose who plays whom. The order comes from a seed over the competition,
//! every ticket placed, and the hash of the first block at or after registration closes:
//!
//! ```text
//! seed = SHA256("5day4cast/pool-seed/v1" || competition_id || sorted(ticket_ids) || block_hash)
//! ```
//!
//! Ids are their 16 bytes, sorted ascending. The block hash is its 32 bytes in consensus order,
//! the reverse of its usual hex display. A Fisher-Yates shuffle of the sorted tickets, driven by the
//! seed, orders them, and the pools take them in that order. Draw `k` of the shuffle is the first 8
//! bytes of `SHA256(seed || k)`, with `k` a big-endian `u64`, and a draw that would bias its index
//! is skipped. The coordinator publishes the inputs, so every player's wallet and the verifier in
//! Keymeld's enclave recompute the same pools.
//!
//! The shuffle decides only who shares a pool. Inside a pool the oracle ranks entries in entry id
//! order, so the earliest entry still wins an exact tie. See `docs/QUEUED_COMPETITIONS.md`.

use dlctix::bitcoin::hashes::Hash;
use dlctix::bitcoin::BlockHash;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::capacity::MAX_COMPETITION_PLAYERS;

pub const SEED_TAG: &[u8] = b"5day4cast/pool-seed/v1";

/// A pool needs a winner and someone to beat.
pub const MIN_POOL_PLAYERS: usize = 2;

/// The most players a pool can hold: [`MAX_COMPETITION_PLAYERS`], 25.
///
/// - Keymeld signs a pool's whole contract after the Arkade batch fixes the funding outpoint and
///   before the batch session ends; otherwise the batch fails. A contract has about 3n MuSig2
///   items with n + 1 signers each, so signing time grows faster than the player count. 25
///   players took 28.4 s in a release build, and the Arkade test server's session lasts 60 s.
/// - The confidential payout path admits at most 25 players. Above that, its bind and signing
///   requests approach Keymeld's payload limit.
/// - The oracle accepts at most 25 entries per event.
///
/// Raising it needs all three to move.
pub const MAX_POOL_PLAYERS: usize = MAX_COMPETITION_PLAYERS;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    #[error("pools need at least {MIN_POOL_PLAYERS} players each")]
    MinimumTooSmall,
    #[error("pools hold at most {MAX_POOL_PLAYERS} players each")]
    MaximumTooLarge,
    #[error("the largest pool must hold at least twice the smallest, less one, so any queue at the minimum or above splits evenly")]
    RangeTooNarrow,
    #[error("ticket {0} is listed more than once")]
    DuplicateTicket(Uuid),
    #[error("the pools do not follow from the seed")]
    NotFromSeed,
}

/// How big a queued competition's pools may be. Rules are checked when built or read, so an
/// invalid one never exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedPoolRules")]
pub struct PoolRules {
    min_players: usize,
    max_players: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedPoolRules {
    min_players: usize,
    max_players: usize,
}

impl TryFrom<UncheckedPoolRules> for PoolRules {
    type Error = PoolError;

    fn try_from(rules: UncheckedPoolRules) -> Result<Self, PoolError> {
        Self::new(rules.min_players, rules.max_players)
    }
}

impl PoolRules {
    /// `min_players` is at least [`MIN_POOL_PLAYERS`] and `max_players` at most
    /// [`MAX_POOL_PLAYERS`]. `max_players` must also be at least `2 * min_players - 1`: then the
    /// even split of any count from `min_players` up never leaves a pool below `min_players`.
    pub fn new(min_players: usize, max_players: usize) -> Result<Self, PoolError> {
        if min_players < MIN_POOL_PLAYERS {
            return Err(PoolError::MinimumTooSmall);
        }
        if max_players > MAX_POOL_PLAYERS {
            return Err(PoolError::MaximumTooLarge);
        }
        if max_players < min_players.saturating_mul(2) - 1 {
            return Err(PoolError::RangeTooNarrow);
        }
        Ok(Self {
            min_players,
            max_players,
        })
    }

    pub fn min_players(&self) -> usize {
        self.min_players
    }

    pub fn max_players(&self) -> usize {
        self.max_players
    }

    /// The size of each pool for `tickets` players, or `None` if they are too few for one.
    /// There are `ceil(tickets / max_players)` pools. The first `tickets mod pools` take one
    /// player more than the rest.
    pub fn sizes(&self, tickets: usize) -> Option<Vec<usize>> {
        if tickets < self.min_players {
            return None;
        }
        let pools = tickets.div_ceil(self.max_players);
        let (base, larger) = (tickets / pools, tickets % pools);
        Some(
            (0..pools)
                .map(|pool| base + usize::from(pool < larger))
                .collect(),
        )
    }
}

/// The pools formed at a queued competition's kickoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Formation {
    /// Too few tickets for a pool; every escrow is refunded.
    TooFew,
    Pools {
        seed: [u8; 32],
        /// Each pool's tickets, in shuffled order.
        pools: Vec<Vec<Uuid>>,
    },
}

/// The seed over the competition, its tickets in any order, and the closing block's hash.
pub fn seed(
    competition_id: Uuid,
    tickets: &[Uuid],
    block_hash: &BlockHash,
) -> Result<[u8; 32], PoolError> {
    let sorted = sorted_tickets(tickets)?;
    let mut hasher = Sha256::new();
    hasher.update(SEED_TAG);
    hasher.update(competition_id.as_bytes());
    for ticket in &sorted {
        hasher.update(ticket.as_bytes());
    }
    hasher.update(block_hash.to_byte_array());
    Ok(hasher.finalize().into())
}

/// Split `tickets` into pools under `rules`.
pub fn form(
    rules: &PoolRules,
    competition_id: Uuid,
    tickets: &[Uuid],
    block_hash: &BlockHash,
) -> Result<Formation, PoolError> {
    let seed = seed(competition_id, tickets, block_hash)?;
    let Some(sizes) = rules.sizes(tickets.len()) else {
        return Ok(Formation::TooFew);
    };
    let mut order = sorted_tickets(tickets)?;
    shuffle(&seed, &mut order);
    let mut rest = order.as_slice();
    let pools = sizes
        .into_iter()
        .map(|size| {
            let (pool, tail) = rest.split_at(size);
            rest = tail;
            pool.to_vec()
        })
        .collect();
    Ok(Formation::Pools { seed, pools })
}

/// Check that `pools` are exactly the pools the seed gives for the tickets they hold together.
pub fn check(
    rules: &PoolRules,
    competition_id: Uuid,
    block_hash: &BlockHash,
    pools: &[Vec<Uuid>],
) -> Result<[u8; 32], PoolError> {
    let tickets: Vec<Uuid> = pools.iter().flatten().copied().collect();
    match form(rules, competition_id, &tickets, block_hash)? {
        Formation::Pools {
            seed,
            pools: expected,
        } if expected == pools => Ok(seed),
        _ => Err(PoolError::NotFromSeed),
    }
}

fn sorted_tickets(tickets: &[Uuid]) -> Result<Vec<Uuid>, PoolError> {
    let mut sorted = tickets.to_vec();
    sorted.sort_unstable();
    if let Some(pair) = sorted.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(PoolError::DuplicateTicket(pair[0]));
    }
    Ok(sorted)
}

/// Durstenfeld's Fisher-Yates, from the last position down.
fn shuffle(seed: &[u8; 32], tickets: &mut [Uuid]) {
    let mut draws = Draws { seed, next: 0 };
    for last in (1..tickets.len()).rev() {
        let pick = draws.below(last as u64 + 1) as usize;
        tickets.swap(last, pick);
    }
}

struct Draws<'a> {
    seed: &'a [u8; 32],
    next: u64,
}

impl Draws<'_> {
    fn draw(&mut self) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(self.seed);
        hasher.update(self.next.to_be_bytes());
        self.next += 1;
        let digest = hasher.finalize();
        u64::from_be_bytes(digest[..8].try_into().expect("a digest has 8 bytes"))
    }

    /// A uniform index below `bound`. Draws at or above the largest multiple of `bound` that
    /// fits are skipped, so no index is favored.
    fn below(&mut self, bound: u64) -> u64 {
        let limit = u64::MAX - u64::MAX % bound;
        loop {
            let draw = self.draw();
            if draw < limit {
                return draw % bound;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::str::FromStr;

    fn competition() -> Uuid {
        Uuid::parse_str("01926f3a-0000-7000-8000-000000000001").unwrap()
    }

    fn block() -> BlockHash {
        BlockHash::from_str("000000000000000000017b0f5bd1e0b6f0c1d0d4f3a8c2e5b9a7d6c4e3f2a1b0")
            .unwrap()
    }

    fn ticket(number: u128) -> Uuid {
        Uuid::from_u128(0x01926f3a_0000_7000_8000_000000000000 | number)
    }

    fn tickets(count: u128) -> Vec<Uuid> {
        (1..=count).map(ticket).collect()
    }

    fn standard() -> PoolRules {
        PoolRules::new(2, 25).unwrap()
    }

    #[test]
    fn rules_leave_no_pool_below_the_minimum() {
        assert_eq!(PoolRules::new(1, 25), Err(PoolError::MinimumTooSmall));
        assert_eq!(PoolRules::new(2, 26), Err(PoolError::MaximumTooLarge));
        assert_eq!(PoolRules::new(13, 24), Err(PoolError::RangeTooNarrow));
        assert!(PoolRules::new(13, 25).is_ok());
        assert!(
            serde_json::from_str::<PoolRules>(r#"{"min_players":1,"max_players":25}"#).is_err()
        );
        assert!(serde_json::from_str::<PoolRules>(r#"{"min_players":2,"max_players":0}"#).is_err());
        let stored: PoolRules =
            serde_json::from_str(r#"{"min_players":3,"max_players":25}"#).unwrap();
        assert_eq!(stored, PoolRules::new(3, 25).unwrap());
    }

    #[test]
    fn sizes_split_evenly_into_the_fewest_pools() {
        let rules = standard();
        assert_eq!(rules.sizes(1), None);
        assert_eq!(rules.sizes(2), Some(vec![2]));
        assert_eq!(rules.sizes(25), Some(vec![25]));
        assert_eq!(rules.sizes(26), Some(vec![13, 13]));
        assert_eq!(rules.sizes(52), Some(vec![18, 17, 17]));
        assert_eq!(rules.sizes(100), Some(vec![25; 4]));
        for (min, max) in [(2, 25), (3, 5), (5, 9), (13, 25), (2, 3)] {
            let rules = PoolRules::new(min, max).unwrap();
            for count in 0..=600 {
                let Some(sizes) = rules.sizes(count) else {
                    assert!(count < min, "{count} players under {rules:?}");
                    continue;
                };
                assert_eq!(sizes.iter().sum::<usize>(), count);
                assert_eq!(sizes.len(), count.div_ceil(max));
                let (small, large) = (sizes.iter().min().unwrap(), sizes.iter().max().unwrap());
                assert!(large - small <= 1 && *large <= max && *small >= min);
                assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]));
            }
        }
    }

    /// Vectors from an independent Python implementation of the rules in the module docs.
    #[test]
    fn matches_the_reference_vectors() {
        let expect = |count: u128, seed_hex: &str, pools: &[&[u128]]| {
            let formation = form(&standard(), competition(), &tickets(count), &block()).unwrap();
            let expected = Formation::Pools {
                seed: hex::decode(seed_hex).unwrap().try_into().unwrap(),
                pools: pools
                    .iter()
                    .map(|pool| pool.iter().copied().map(ticket).collect())
                    .collect(),
            };
            assert_eq!(formation, expected, "{count} tickets");
        };
        expect(
            2,
            "2d6d77ff4b40de629271c0f3ea275faf5091b2b401b23803b1ba1c5987cd3c14",
            &[&[0x02, 0x01]],
        );
        expect(
            3,
            "d8af7d76010e7702fa4757e64e8431ac6521b33d79d92d8ac19d345d011201f5",
            &[&[0x02, 0x03, 0x01]],
        );
        expect(
            26,
            "810b8ecd890654431c30b3a2f86cc1ef6f951f94dab30887196554d9e064c0a4",
            &[
                &[
                    0x14, 0x0e, 0x16, 0x11, 0x0f, 0x04, 0x05, 0x0a, 0x15, 0x12, 0x13, 0x01, 0x18,
                ],
                &[
                    0x08, 0x19, 0x10, 0x1a, 0x0d, 0x07, 0x17, 0x0b, 0x09, 0x06, 0x03, 0x0c, 0x02,
                ],
            ],
        );
        assert_eq!(
            form(&standard(), competition(), &tickets(1), &block()).unwrap(),
            Formation::TooFew
        );
        assert_eq!(
            hex::encode(seed(competition(), &tickets(1), &block()).unwrap()),
            "d36c74244bf7d40d78a84f4a9b43613bcbae178492816e0a0b375ee35dd012a7"
        );
    }

    #[test]
    fn the_seed_ignores_ticket_order_but_not_the_block() {
        let mut shuffled = tickets(40);
        shuffled.reverse();
        shuffled.swap(3, 17);
        assert_eq!(
            form(&standard(), competition(), &shuffled, &block()).unwrap(),
            form(&standard(), competition(), &tickets(40), &block()).unwrap()
        );
        let other = BlockHash::from_byte_array([7; 32]);
        assert_ne!(
            seed(competition(), &tickets(40), &block()).unwrap(),
            seed(competition(), &tickets(40), &other).unwrap()
        );
        assert_ne!(
            seed(competition(), &tickets(40), &block()).unwrap(),
            seed(Uuid::now_v7(), &tickets(40), &block()).unwrap()
        );
    }

    #[test]
    fn a_ticket_counts_once() {
        let mut listed = tickets(5);
        listed.push(ticket(3));
        assert_eq!(
            form(&standard(), competition(), &listed, &block()),
            Err(PoolError::DuplicateTicket(ticket(3)))
        );
    }

    #[test]
    fn every_ticket_lands_in_exactly_one_pool() {
        for count in [2, 25, 26, 51, 99, 100, 250] {
            let Formation::Pools { pools, .. } =
                form(&standard(), competition(), &tickets(count), &block()).unwrap()
            else {
                panic!("{count} tickets form pools");
            };
            let placed: BTreeSet<Uuid> = pools.iter().flatten().copied().collect();
            assert_eq!(placed, tickets(count).into_iter().collect());
            assert_eq!(pools.iter().map(Vec::len).sum::<usize>(), count as usize);
        }
    }

    #[test]
    fn check_accepts_only_the_seeded_pools() {
        let Formation::Pools { seed, pools } =
            form(&standard(), competition(), &tickets(60), &block()).unwrap()
        else {
            panic!("60 tickets form pools");
        };
        assert_eq!(
            check(&standard(), competition(), &block(), &pools),
            Ok(seed)
        );

        let mut swapped = pools.clone();
        let moved = swapped[0][0];
        swapped[0][0] = swapped[1][0];
        swapped[1][0] = moved;
        let mut reordered = pools.clone();
        reordered.swap(0, 2);
        let mut dropped = pools.clone();
        dropped[2].pop();
        let mut added = pools.clone();
        added[1].push(ticket(999));
        for (name, candidate) in [
            ("swapped players", swapped),
            ("reordered pools", reordered),
            ("dropped ticket", dropped),
            ("added ticket", added),
        ] {
            assert_eq!(
                check(&standard(), competition(), &block(), &candidate),
                Err(PoolError::NotFromSeed),
                "{name}"
            );
        }
        assert_eq!(
            check(
                &standard(),
                competition(),
                &BlockHash::from_byte_array([7; 32]),
                &pools
            ),
            Err(PoolError::NotFromSeed)
        );
    }

    /// Each order of three tickets should come up about a sixth of the time across blocks.
    #[test]
    fn the_shuffle_is_not_skewed() {
        let three = tickets(3);
        let mut counts: BTreeMap<Vec<Uuid>, usize> = BTreeMap::new();
        for index in 0u32..6_000 {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&index.to_be_bytes());
            let Formation::Pools { pools, .. } = form(
                &standard(),
                competition(),
                &three,
                &BlockHash::from_byte_array(hash),
            )
            .unwrap() else {
                panic!("three tickets form a pool");
            };
            *counts.entry(pools[0].clone()).or_default() += 1;
        }
        assert_eq!(counts.len(), 6);
        assert!(
            counts.values().all(|&count| (850..=1_150).contains(&count)),
            "{counts:?}"
        );
    }
}
