//! Persisted pacing for payment reconciliation and refund claims.
//!
//! Reserve the next attempt before a network call. Both an empty successful
//! listing and an error must wait; neither proves that money may be sent again.

use rand08::Rng;

/// Equal jitter, with a ten-second floor, doubling to a five-minute ceiling.
/// `attempt` is one for the first lookup, which may run immediately.
pub fn delay_secs(attempt: u32, rng: &mut impl Rng) -> i64 {
    let ceiling = (20_u64 << attempt.saturating_sub(1).min(4)).min(300);
    rng.gen_range(ceiling / 2..=ceiling) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand08::SeedableRng;
    use std::collections::BTreeSet;

    #[test]
    fn delays_have_a_floor_grow_and_stay_jittered_at_the_cap() {
        let mut rng = rand08::rngs::StdRng::seed_from_u64(42);
        for (attempt, low, high) in [
            (1, 10, 20),
            (2, 20, 40),
            (3, 40, 80),
            (4, 80, 160),
            (5, 150, 300),
            (u32::MAX, 150, 300),
        ] {
            let samples: BTreeSet<_> = (0..128).map(|_| delay_secs(attempt, &mut rng)).collect();
            assert!(samples.iter().all(|delay| (low..=high).contains(delay)));
            assert!(samples.len() > 1);
        }
    }
}
