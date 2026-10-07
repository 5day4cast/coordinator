//! Proof of work for new accounts. Accounts are free and the site is public, so a bot could
//! create them by the thousand; limits per client address cannot stop that without locking out
//! a crowd behind one conference NAT. Instead the browser spends about a second of CPU, in the
//! background while the visitor types, before an account is created: nothing to a person, a
//! real cost per account to a bot.
//!
//! The scheme, shared with the Satchel wallet:
//! - challenge: 16 random bytes ‖ `expires_at` (u64 big-endian) ‖ difficulty (u8) ‖ the first
//!   16 bytes of HMAC-SHA256(secret, the 25 bytes before it); 41 bytes, base64url without
//!   padding, good for ten minutes;
//! - solution: a u64 nonce such that SHA-256(challenge ‖ nonce big-endian) starts with at least
//!   `difficulty` zero bits;
//! - each challenge creates at most one account.
//!
//! Issuing is stateless: the secret is random per process, so a restart voids outstanding
//! challenges and the browser fetches another. Only spent challenges are remembered, until
//! they expire. The difficulty is global, never per client address: `base_bits`, plus one bit
//! for every `step_signups` accounts created in the last hour, up to `max_bits`.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use super::UserInfo;
use crate::{
    config::PowSettings,
    domain::Error,
    metrics::{SIGNUP_POW_CHECKS, SIGNUP_POW_DIFFICULTY},
};

/// How long a challenge may be solved and spent after it is issued.
pub const CHALLENGE_LIFETIME_SECS: u64 = 600;
/// The window of account creations the difficulty follows.
pub const SIGNUP_WINDOW_SECS: i64 = 3600;
/// How long a count of recent account creations is reused, so issuing challenges does not
/// query the database each time.
const SIGNUP_COUNT_TTL: Duration = Duration::from_secs(15);
/// How often spent challenges past their expiry are forgotten.
const PRUNE_EVERY_SECS: u64 = 60;

const RANDOM_LEN: usize = 16;
/// The bytes the tag covers: the random bytes, `expires_at` and the difficulty.
const SIGNED_LEN: usize = RANDOM_LEN + 8 + 1;
const TAG_LEN: usize = 16;
const CHALLENGE_LEN: usize = SIGNED_LEN + TAG_LEN;

type HmacSha256 = Hmac<Sha256>;

/// What `POST /api/v1/users/pow` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowChallenge {
    /// The challenge bytes, base64url without padding.
    pub challenge: String,
    /// Leading zero bits the solution's hash needs.
    pub difficulty: u8,
    /// Unix seconds after which the challenge is refused.
    pub expires_at: u64,
}

/// Why an account creation's proof of work was refused. Each message is one sentence for the
/// visitor; the browser solves a fresh challenge and tries once more before showing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PowRejection {
    #[error("Signing up needs a proof of work from this page; reload it and try again")]
    Missing,
    #[error("The sign-up proof of work is not valid; try again")]
    Malformed,
    #[error("The sign-up proof of work was not issued here; try again")]
    Forged,
    #[error("The sign-up proof of work expired; try again")]
    Expired,
    #[error("Sign-ups are busy, so the proof of work got harder; try again")]
    TooEasy,
    #[error("The sign-up proof of work does not solve its challenge; try again")]
    WrongNonce,
    #[error("The sign-up proof of work was already used; try again")]
    Reused,
}

impl PowRejection {
    /// Its `result` label on `coordinator_signup_pow_checks_total`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Malformed => "malformed",
            Self::Forged => "forged",
            Self::Expired => "expired",
            Self::TooEasy => "too_easy",
            Self::WrongNonce => "wrong_nonce",
            Self::Reused => "reused",
        }
    }
}

/// The proof of work an account creation sends: `pow_challenge`, and `pow_nonce` in decimal.
#[derive(Debug, Default, Deserialize)]
pub struct PowProof {
    #[serde(default)]
    pub pow_challenge: Option<String>,
    #[serde(default)]
    pub pow_nonce: Option<String>,
}

/// Spent challenges by their expiry, and when they were last pruned.
#[derive(Default)]
struct Spent {
    challenges: HashMap<[u8; CHALLENGE_LEN], u64>,
    pruned_at: u64,
}

/// Issues and checks the proofs of work new accounts carry.
pub struct SignupPow {
    settings: PowSettings,
    secret: [u8; 32],
    spent: Mutex<Spent>,
    /// Accounts created in the last hour as last counted, and when.
    signups: Mutex<Option<(Instant, u64)>>,
    /// Whether checks count on the sign-up metrics. A fixed-difficulty instance, like the
    /// feedback form's, leaves them to sign-ups.
    signup_metrics: bool,
}

impl SignupPow {
    pub fn new(settings: PowSettings) -> Self {
        let mut secret = [0u8; 32];
        rand::rng().fill(&mut secret);
        Self::with_secret(settings, secret)
    }

    /// An enabled instance whose challenges always take `bits`, outside the sign-up metrics.
    pub fn fixed(bits: u8) -> Self {
        let mut secret = [0u8; 32];
        rand::rng().fill(&mut secret);
        Self::fixed_with_secret(bits, secret)
    }

    fn fixed_with_secret(bits: u8, secret: [u8; 32]) -> Self {
        Self {
            settings: PowSettings {
                enabled: true,
                base_bits: bits,
                max_bits: bits,
                step_signups: 1,
            },
            secret,
            spent: Mutex::new(Spent::default()),
            signups: Mutex::new(None),
            signup_metrics: false,
        }
    }

    fn with_secret(settings: PowSettings, secret: [u8; 32]) -> Self {
        SIGNUP_POW_DIFFICULTY.set(if settings.enabled {
            i64::from(settings.base_bits)
        } else {
            0
        });
        Self {
            settings,
            secret,
            spent: Mutex::new(Spent::default()),
            signups: Mutex::new(None),
            signup_metrics: true,
        }
    }

    /// Whether account creations must carry a proof of work.
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// The difficulty required while `recent_signups` accounts were created in the last hour.
    pub fn difficulty(&self, recent_signups: u64) -> u8 {
        let extra = recent_signups / self.settings.step_signups.max(1);
        let bits = u64::from(self.settings.base_bits).saturating_add(extra);
        // At most max_bits, which is a u8.
        bits.min(u64::from(self.settings.max_bits)) as u8
    }

    /// The difficulty required now, counting recent account creations at most every
    /// [`SIGNUP_COUNT_TTL`].
    pub async fn required_bits(&self, users: &UserInfo) -> Result<u8, Error> {
        let cached = *self.signups.lock().unwrap_or_else(|e| e.into_inner());
        let recent = match cached {
            Some((at, count)) if at.elapsed() < SIGNUP_COUNT_TTL => count,
            _ => {
                let since = OffsetDateTime::now_utc() - time::Duration::seconds(SIGNUP_WINDOW_SECS);
                let count = users.count_signups_since(since).await?;
                *self.signups.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((Instant::now(), count));
                count
            }
        };
        let bits = self.difficulty(recent);
        SIGNUP_POW_DIFFICULTY.set(i64::from(bits));
        Ok(bits)
    }

    /// A fresh challenge at `difficulty`, issued at `now` (Unix seconds).
    pub fn issue(&self, difficulty: u8, now: u64) -> PowChallenge {
        let mut random = [0u8; RANDOM_LEN];
        rand::rng().fill(&mut random);
        self.issue_with(random, difficulty, now)
    }

    fn issue_with(&self, random: [u8; RANDOM_LEN], difficulty: u8, now: u64) -> PowChallenge {
        let expires_at = now.saturating_add(CHALLENGE_LIFETIME_SECS);
        let mut bytes = [0u8; CHALLENGE_LEN];
        bytes[..RANDOM_LEN].copy_from_slice(&random);
        bytes[RANDOM_LEN..RANDOM_LEN + 8].copy_from_slice(&expires_at.to_be_bytes());
        bytes[SIGNED_LEN - 1] = difficulty;
        let tag = self.mac(&bytes[..SIGNED_LEN]).finalize().into_bytes();
        bytes[SIGNED_LEN..].copy_from_slice(&tag[..TAG_LEN]);
        PowChallenge {
            challenge: URL_SAFE_NO_PAD.encode(bytes),
            difficulty,
            expires_at,
        }
    }

    fn mac(&self, signed: &[u8]) -> HmacSha256 {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a key of any length");
        mac.update(signed);
        mac
    }

    /// Check an account creation's proof of work and spend its challenge, counting the result
    /// on `coordinator_signup_pow_checks_total`. See [`Self::check`].
    pub fn verify(&self, proof: &PowProof, required: u8, now: u64) -> Result<(), PowRejection> {
        let checked = match (&proof.pow_challenge, &proof.pow_nonce) {
            (Some(challenge), Some(nonce)) => nonce
                .parse()
                .map_err(|_| PowRejection::Malformed)
                .and_then(|nonce| self.check(challenge, nonce, required, now)),
            _ => Err(PowRejection::Missing),
        };
        if self.signup_metrics {
            let label = match checked {
                Ok(()) => "verified",
                Err(rejection) => rejection.label(),
            };
            SIGNUP_POW_CHECKS.with_label_values(&[label]).inc();
        }
        checked
    }

    /// Check a solution to one of this process's challenges and spend the challenge. It must
    /// be unexpired at `now`, issued at `required` bits or more, solved by `nonce`, and not
    /// spent before.
    fn check(
        &self,
        challenge: &str,
        nonce: u64,
        required: u8,
        now: u64,
    ) -> Result<(), PowRejection> {
        let bytes: [u8; CHALLENGE_LEN] = URL_SAFE_NO_PAD
            .decode(challenge)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(PowRejection::Malformed)?;
        self.mac(&bytes[..SIGNED_LEN])
            .verify_truncated_left(&bytes[SIGNED_LEN..])
            .map_err(|_| PowRejection::Forged)?;
        let mut expires_at = [0u8; 8];
        expires_at.copy_from_slice(&bytes[RANDOM_LEN..RANDOM_LEN + 8]);
        let expires_at = u64::from_be_bytes(expires_at);
        if now >= expires_at {
            return Err(PowRejection::Expired);
        }
        let difficulty = bytes[SIGNED_LEN - 1];
        if difficulty < required {
            return Err(PowRejection::TooEasy);
        }
        if !solves(&bytes, nonce, difficulty) {
            return Err(PowRejection::WrongNonce);
        }

        let mut spent = self.spent.lock().unwrap_or_else(|e| e.into_inner());
        if now >= spent.pruned_at.saturating_add(PRUNE_EVERY_SECS) {
            spent.challenges.retain(|_, expiry| *expiry > now);
            spent.pruned_at = now;
        }
        if spent.challenges.contains_key(&bytes) {
            return Err(PowRejection::Reused);
        }
        spent.challenges.insert(bytes, expires_at);
        Ok(())
    }
}

/// Whether SHA-256(`challenge` ‖ `nonce` big-endian) starts with at least `difficulty` zero bits.
fn solves(challenge: &[u8], nonce: u64, difficulty: u8) -> bool {
    let hash = Sha256::new()
        .chain_update(challenge)
        .chain_update(nonce.to_be_bytes())
        .finalize();
    leading_zero_bits(&hash) >= u32::from(difficulty)
}

fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut bits = 0;
    for byte in bytes {
        bits += byte.leading_zeros();
        if *byte != 0 {
            break;
        }
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_791_400_000;

    fn settings(base_bits: u8) -> PowSettings {
        PowSettings {
            enabled: true,
            base_bits,
            max_bits: 22,
            step_signups: 200,
        }
    }

    fn pow(base_bits: u8) -> SignupPow {
        SignupPow::with_secret(settings(base_bits), [0x42; 32])
    }

    fn proof(challenge: &str, nonce: u64) -> PowProof {
        PowProof {
            pow_challenge: Some(challenge.to_owned()),
            pow_nonce: Some(nonce.to_string()),
        }
    }

    /// The smallest nonce solving `challenge` at its own difficulty.
    fn solve(challenge: &PowChallenge) -> u64 {
        let bytes = URL_SAFE_NO_PAD.decode(&challenge.challenge).unwrap();
        (0..)
            .find(|nonce| solves(&bytes, *nonce, challenge.difficulty))
            .unwrap()
    }

    /// The vector `static/pow-worker.js` notes too, and the Satchel wallet checks: with the
    /// secret 0x42 × 32, the random bytes 00..0f, `expires_at` 1791400600 and difficulty 16,
    /// the challenge is the string below, and nonce 91039 is the first to solve it
    /// (SHA-256 0000ef4d…, exactly 16 zero bits).
    #[test]
    fn a_fixed_challenge_and_nonce_pass_at_sixteen_bits() {
        const CHALLENGE: &str = "AAECAwQFBgcICQoLDA0ODwAAAABqxpqYEAMhk91h_us6MSi5qOUB7I8";
        let pow = pow(16);
        let random: [u8; 16] = std::array::from_fn(|i| i as u8);
        let issued = pow.issue_with(random, 16, NOW);
        assert_eq!(
            issued,
            PowChallenge {
                challenge: CHALLENGE.into(),
                difficulty: 16,
                expires_at: 1_791_400_600,
            }
        );
        let bytes = URL_SAFE_NO_PAD.decode(CHALLENGE).unwrap();
        assert_eq!(bytes.len(), 41);
        assert_eq!(
            hex::encode(&bytes),
            "000102030405060708090a0b0c0d0e0f000000006ac69a9810032193dd61feeb3a3128b9a8e501ec8f"
        );
        assert!(solves(&bytes, 91_039, 16));
        assert!(!solves(&bytes, 91_039, 17));
        assert!(!solves(&bytes, 91_040, 16));
        assert_eq!(solve(&issued), 91_039);
        // A nonce past 2^32 counts its high word: 4294971180 solves it at 12 bits.
        assert!(solves(&bytes, 4_294_971_180, 12));
        assert_eq!(pow.verify(&proof(CHALLENGE, 91_039), 16, NOW), Ok(()));
    }

    #[test]
    fn a_solved_challenge_creates_one_account() {
        let pow = pow(8);
        let challenge = pow.issue(8, NOW);
        assert_eq!(challenge.expires_at, NOW + CHALLENGE_LIFETIME_SECS);
        let nonce = solve(&challenge);
        assert_eq!(
            pow.verify(&proof(&challenge.challenge, nonce), 8, NOW + 1),
            Ok(())
        );
        assert_eq!(
            pow.verify(&proof(&challenge.challenge, nonce), 8, NOW + 2),
            Err(PowRejection::Reused)
        );
        // Another challenge is unaffected.
        let other = pow.issue(8, NOW);
        assert_ne!(other.challenge, challenge.challenge);
        assert_eq!(
            pow.verify(&proof(&other.challenge, solve(&other)), 8, NOW),
            Ok(())
        );
    }

    #[test]
    fn wrong_expired_tampered_and_too_easy_solutions_are_refused() {
        let pow = pow(8);
        let challenge = pow.issue(8, NOW);
        let nonce = solve(&challenge);
        let wrong = (nonce + 1..)
            .find(|n| {
                !solves(
                    &URL_SAFE_NO_PAD.decode(&challenge.challenge).unwrap(),
                    *n,
                    8,
                )
            })
            .unwrap();
        assert_eq!(
            pow.verify(&proof(&challenge.challenge, wrong), 8, NOW),
            Err(PowRejection::WrongNonce)
        );
        assert_eq!(
            pow.verify(
                &proof(&challenge.challenge, nonce),
                8,
                NOW + CHALLENGE_LIFETIME_SECS
            ),
            Err(PowRejection::Expired)
        );
        // Issued at 8 bits while 9 are required now.
        assert_eq!(
            pow.verify(&proof(&challenge.challenge, nonce), 9, NOW),
            Err(PowRejection::TooEasy)
        );

        // Raising its difficulty or extending its life breaks the tag.
        let mut bytes = URL_SAFE_NO_PAD.decode(&challenge.challenge).unwrap();
        bytes[SIGNED_LEN - 1] = 20;
        assert_eq!(
            pow.verify(&proof(&URL_SAFE_NO_PAD.encode(&bytes), nonce), 8, NOW),
            Err(PowRejection::Forged)
        );
        let mut bytes = URL_SAFE_NO_PAD.decode(&challenge.challenge).unwrap();
        bytes[RANDOM_LEN + 7] ^= 1;
        assert_eq!(
            pow.verify(&proof(&URL_SAFE_NO_PAD.encode(&bytes), nonce), 8, NOW),
            Err(PowRejection::Forged)
        );
        // Another process's challenges are not this one's.
        let elsewhere = SignupPow::with_secret(settings(8), [0x24; 32]);
        assert_eq!(
            elsewhere.verify(&proof(&challenge.challenge, nonce), 8, NOW),
            Err(PowRejection::Forged)
        );
        for malformed in ["", "not base64!", &challenge.challenge[..40], "AAAA"] {
            assert_eq!(
                pow.verify(&proof(malformed, nonce), 8, NOW),
                Err(PowRejection::Malformed),
                "{malformed}"
            );
        }
        let padded = format!("{}=", challenge.challenge);
        assert_eq!(
            pow.verify(&proof(&padded, nonce), 8, NOW),
            Err(PowRejection::Malformed)
        );

        // None of the refusals spent it.
        assert_eq!(
            pow.verify(&proof(&challenge.challenge, nonce), 8, NOW),
            Ok(())
        );
    }

    #[test]
    fn a_missing_or_unreadable_proof_is_refused() {
        let pow = pow(1);
        let challenge = pow.issue(1, NOW);
        let nonce = solve(&challenge);
        let sent = |body: serde_json::Value| -> PowProof { serde_json::from_value(body).unwrap() };
        for (body, refused) in [
            (serde_json::json!({}), PowRejection::Missing),
            (
                serde_json::json!({ "pow_challenge": challenge.challenge }),
                PowRejection::Missing,
            ),
            (
                serde_json::json!({ "pow_nonce": nonce.to_string() }),
                PowRejection::Missing,
            ),
            (
                serde_json::json!({ "pow_challenge": challenge.challenge, "pow_nonce": "12ab" }),
                PowRejection::Malformed,
            ),
            (
                serde_json::json!({ "pow_challenge": challenge.challenge, "pow_nonce": "18446744073709551616" }),
                PowRejection::Malformed,
            ),
        ] {
            assert_eq!(
                pow.verify(&sent(body.clone()), 1, NOW),
                Err(refused),
                "{body}"
            );
        }
        assert_eq!(
            pow.verify(
                &sent(serde_json::json!({
                    "pow_challenge": challenge.challenge,
                    "pow_nonce": nonce.to_string(),
                })),
                1,
                NOW
            ),
            Ok(())
        );
    }

    #[test]
    fn difficulty_rises_a_bit_per_step_of_recent_signups_up_to_the_cap() {
        let pow = pow(18);
        for (recent, bits) in [
            (0, 18),
            (199, 18),
            (200, 19),
            (399, 19),
            (400, 20),
            (800, 22),
            (10_000, 22),
            (u64::MAX, 22),
        ] {
            assert_eq!(pow.difficulty(recent), bits, "{recent}");
        }
        let flat = SignupPow::with_secret(
            PowSettings {
                base_bits: 4,
                max_bits: 4,
                ..settings(4)
            },
            [0; 32],
        );
        assert_eq!(flat.difficulty(1_000_000), 4);
    }

    #[test]
    fn spent_challenges_are_forgotten_once_expired() {
        let pow = pow(1);
        let challenge = pow.issue(1, NOW);
        pow.verify(&proof(&challenge.challenge, solve(&challenge)), 1, NOW)
            .unwrap();
        assert_eq!(pow.spent.lock().unwrap().challenges.len(), 1);
        let later = NOW + CHALLENGE_LIFETIME_SECS + PRUNE_EVERY_SECS;
        let fresh = pow.issue(1, later);
        pow.verify(&proof(&fresh.challenge, solve(&fresh)), 1, later)
            .unwrap();
        let spent = pow.spent.lock().unwrap();
        assert_eq!(spent.challenges.len(), 1);
        assert_eq!(spent.challenges.values().next(), Some(&fresh.expires_at));
    }

    #[test]
    fn a_fixed_instance_keeps_its_difficulty_and_spends_each_challenge_once() {
        let fixed = SignupPow::fixed_with_secret(6, [0x17; 32]);
        assert!(fixed.enabled());
        assert_eq!(fixed.difficulty(0), 6);
        assert_eq!(fixed.difficulty(u64::MAX), 6);
        let challenge = fixed.issue(fixed.difficulty(0), NOW);
        let nonce = solve(&challenge);
        assert_eq!(
            fixed.verify(&proof(&challenge.challenge, nonce), 6, NOW),
            Ok(())
        );
        assert_eq!(
            fixed.verify(&proof(&challenge.challenge, nonce), 6, NOW),
            Err(PowRejection::Reused)
        );
        // Challenges from the sign-up instance are not its own.
        let signups = pow(6);
        let theirs = signups.issue(6, NOW);
        assert_eq!(
            fixed.verify(&proof(&theirs.challenge, solve(&theirs)), 6, NOW),
            Err(PowRejection::Forged)
        );
    }

    #[test]
    fn leading_zero_bits_count_across_bytes() {
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x10]), 19);
        assert_eq!(leading_zero_bits(&[0x00, 0x00]), 16);
    }
}
