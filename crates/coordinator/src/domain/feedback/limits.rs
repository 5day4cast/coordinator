//! How many feedback messages are accepted, per browser session, per client address and for
//! the whole site. Counts are kept in memory over sliding windows; a restart forgets them,
//! which the proof of work and the site-wide cap make harmless.

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::Mutex,
};

/// At most `max` messages in any `window_secs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitRule {
    pub max: usize,
    pub window_secs: u64,
}

/// Per browser session: a person writing more than this is better served by email.
pub const PER_SESSION: LimitRule = LimitRule {
    max: 3,
    window_secs: 3600,
};
/// Per client address. A conference or office can put many people behind one address.
pub const PER_ADDRESS: LimitRule = LimitRule {
    max: 20,
    window_secs: 600,
};
/// For the whole site, so a flood cannot bury the operator.
pub const SITE_WIDE: LimitRule = LimitRule {
    max: 60,
    window_secs: 3600,
};
/// Messages sent without a proof of work, from browsers without JavaScript. They share this
/// smaller allowance, inside the site-wide one.
pub const WITHOUT_WORK: LimitRule = LimitRule {
    max: 10,
    window_secs: 3600,
};

/// Keys kept per kind before idle ones are forgotten.
const MAX_KEYS: usize = 10_000;

/// Which limit refused a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Limited {
    #[error("session")]
    Session,
    #[error("address")]
    Address,
    #[error("site")]
    Site,
}

/// Timestamps (Unix seconds) of accepted messages, per key.
#[derive(Default)]
struct Windows {
    keys: HashMap<String, VecDeque<u64>>,
}

impl Windows {
    fn allows(&mut self, key: &str, rule: LimitRule, now: u64) -> bool {
        match self.keys.get_mut(key) {
            Some(times) => {
                let since = now.saturating_sub(rule.window_secs);
                while times.front().is_some_and(|at| *at <= since) {
                    times.pop_front();
                }
                times.len() < rule.max
            }
            None => rule.max > 0,
        }
    }

    fn record(&mut self, key: &str, rule: LimitRule, now: u64) {
        if self.keys.len() >= MAX_KEYS && !self.keys.contains_key(key) {
            let since = now.saturating_sub(rule.window_secs);
            self.keys
                .retain(|_, times| times.back().is_some_and(|at| *at > since));
        }
        self.keys.entry(key.to_owned()).or_default().push_back(now);
    }
}

#[derive(Default)]
struct Counts {
    sessions: Windows,
    addresses: Windows,
    site: Windows,
    without_work: Windows,
}

/// The feedback limits. A message is counted only when every limit allows it.
pub struct FeedbackLimits {
    session: LimitRule,
    address: LimitRule,
    site: LimitRule,
    without_work: LimitRule,
    counts: Mutex<Counts>,
}

impl Default for FeedbackLimits {
    fn default() -> Self {
        Self::new(PER_SESSION, PER_ADDRESS, SITE_WIDE, WITHOUT_WORK)
    }
}

impl FeedbackLimits {
    pub fn new(
        session: LimitRule,
        address: LimitRule,
        site: LimitRule,
        without_work: LimitRule,
    ) -> Self {
        Self {
            session,
            address,
            site,
            without_work,
            counts: Mutex::new(Counts::default()),
        }
    }

    /// Count a message from `sid` (when the browser has one) at `ip`, sent at `now` (Unix
    /// seconds), or say which limit refuses it. `with_work` is whether it carried a proof of
    /// work.
    pub fn admit(
        &self,
        sid: Option<&str>,
        ip: IpAddr,
        with_work: bool,
        now: u64,
    ) -> Result<(), Limited> {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let address = ip.to_canonical().to_string();
        if let Some(sid) = sid {
            if !counts.sessions.allows(sid, self.session, now) {
                return Err(Limited::Session);
            }
        }
        if !counts.addresses.allows(&address, self.address, now) {
            return Err(Limited::Address);
        }
        if !counts.site.allows("", self.site, now)
            || (!with_work && !counts.without_work.allows("", self.without_work, now))
        {
            return Err(Limited::Site);
        }
        if let Some(sid) = sid {
            counts.sessions.record(sid, self.session, now);
        }
        counts.addresses.record(&address, self.address, now);
        counts.site.record("", self.site, now);
        if !with_work {
            counts.without_work.record("", self.without_work, now);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_791_400_000;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([203, 0, 113, last])
    }

    #[test]
    fn a_session_sends_three_an_hour() {
        let limits = FeedbackLimits::default();
        for i in 0..3 {
            assert_eq!(
                limits.admit(Some("tab-aaaaaaaaaaaaaaaa"), ip(1), true, NOW + i),
                Ok(())
            );
        }
        assert_eq!(
            limits.admit(Some("tab-aaaaaaaaaaaaaaaa"), ip(1), true, NOW + 10),
            Err(Limited::Session)
        );
        // Another tab behind the same address is unaffected.
        assert_eq!(
            limits.admit(Some("tab-bbbbbbbbbbbbbbbb"), ip(1), true, NOW + 10),
            Ok(())
        );
        // An hour after the first, the session may send again.
        assert_eq!(
            limits.admit(Some("tab-aaaaaaaaaaaaaaaa"), ip(1), true, NOW + 3600),
            Ok(())
        );
    }

    #[test]
    fn a_crowd_behind_one_address_gets_twenty_in_ten_minutes() {
        let limits = FeedbackLimits::default();
        for i in 0..20 {
            let sid = format!("tab-{i:016}");
            assert_eq!(
                limits.admit(Some(&sid), ip(7), true, NOW + i),
                Ok(()),
                "{i}"
            );
        }
        assert_eq!(
            limits.admit(Some("tab-zzzzzzzzzzzzzzzz"), ip(7), true, NOW + 30),
            Err(Limited::Address)
        );
        assert_eq!(
            limits.admit(Some("tab-zzzzzzzzzzzzzzzz"), ip(8), true, NOW + 30),
            Ok(())
        );
        assert_eq!(
            limits.admit(Some("tab-yyyyyyyyyyyyyyyy"), ip(7), true, NOW + 600),
            Ok(())
        );
    }

    #[test]
    fn the_site_takes_sixty_an_hour_and_ten_without_work() {
        let limits = FeedbackLimits::default();
        for i in 0..10u8 {
            assert_eq!(limits.admit(None, ip(i), false, NOW), Ok(()), "{i}");
        }
        assert_eq!(limits.admit(None, ip(100), false, NOW), Err(Limited::Site));
        for i in 10..60u8 {
            assert_eq!(limits.admit(None, ip(i), true, NOW), Ok(()), "{i}");
        }
        assert_eq!(limits.admit(None, ip(200), true, NOW), Err(Limited::Site));
        assert_eq!(limits.admit(None, ip(200), true, NOW + 3600), Ok(()));
    }

    #[test]
    fn a_refused_message_is_not_counted() {
        let limits = FeedbackLimits::new(
            LimitRule {
                max: 1,
                window_secs: 60,
            },
            LimitRule {
                max: 5,
                window_secs: 60,
            },
            LimitRule {
                max: 2,
                window_secs: 60,
            },
            WITHOUT_WORK,
        );
        assert_eq!(
            limits.admit(Some("tab-aaaaaaaaaaaaaaaa"), ip(1), true, NOW),
            Ok(())
        );
        // Refused by its session: the site-wide count stays at one.
        assert_eq!(
            limits.admit(Some("tab-aaaaaaaaaaaaaaaa"), ip(1), true, NOW),
            Err(Limited::Session)
        );
        assert_eq!(
            limits.admit(Some("tab-bbbbbbbbbbbbbbbb"), ip(2), true, NOW),
            Ok(())
        );
        assert_eq!(
            limits.admit(Some("tab-cccccccccccccccc"), ip(3), true, NOW),
            Err(Limited::Site)
        );
    }

    #[test]
    fn idle_keys_are_forgotten_when_the_map_is_full() {
        let mut windows = Windows::default();
        let rule = LimitRule {
            max: 1,
            window_secs: 60,
        };
        for i in 0..MAX_KEYS {
            windows.record(&i.to_string(), rule, NOW);
        }
        windows.record("fresh", rule, NOW + 61);
        assert_eq!(windows.keys.len(), 1);
    }
}
