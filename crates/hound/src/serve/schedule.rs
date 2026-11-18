//! When `hound serve` enriches: after the index changed, at most every
//! [`ENRICH_EVERY`] seconds, and not again soon for a session that just failed.
//! Pure state, given the time; the upkeep thread owns it.

use std::collections::HashMap;

use hound_core::model::SessionKey;

/// Least time between two enrichment passes, in seconds.
pub(crate) const ENRICH_EVERY: i64 = 10 * 60;
/// How long a session that failed once is left alone, in seconds.
pub(crate) const FIRST_COOLDOWN: i64 = 60 * 60;
/// The longest a failing session is left alone, in seconds.
pub(crate) const MAX_COOLDOWN: i64 = 24 * 60 * 60;

/// Whether an enrichment pass is due at `now`: the index changed since the
/// last pass covered it (`changed_at`, Unix seconds), and the last pass, if
/// any, was at least [`ENRICH_EVERY`] ago.
pub(crate) fn enrich_due(changed_at: Option<i64>, last_pass: Option<i64>, now: i64) -> bool {
    changed_at.is_some() && last_pass.is_none_or(|t| now.saturating_sub(t) >= ENRICH_EVERY)
}

/// When the index last changed and when the last pass ran.
#[derive(Debug)]
pub(crate) struct EnrichSchedule {
    /// A session must be quiet this long before it is enriched.
    quiet_secs: i64,
    /// The latest change no pass has covered yet.
    changed_at: Option<i64>,
    last_pass: Option<i64>,
}

impl EnrichSchedule {
    pub(crate) fn new(quiet_secs: i64) -> EnrichSchedule {
        EnrichSchedule {
            quiet_secs,
            changed_at: None,
            last_pass: None,
        }
    }

    /// A refresh at `now` added or updated at least one session.
    pub(crate) fn changed(&mut self, now: i64) {
        self.changed_at = Some(now);
    }

    pub(crate) fn due(&self, now: i64) -> bool {
        enrich_due(self.changed_at, self.last_pass, now)
    }

    /// A pass ran at `now`. It covers the latest change once sessions changed
    /// then have been quiet long enough to be enriched; until then, passes
    /// keep coming every [`ENRICH_EVERY`].
    pub(crate) fn passed(&mut self, now: i64) {
        self.last_pass = Some(now);
        if self
            .changed_at
            .is_some_and(|c| now.saturating_sub(c) >= self.quiet_secs)
        {
            self.changed_at = None;
        }
    }
}

/// Sessions whose enrichment failed recently, and until when each is left
/// alone: [`FIRST_COOLDOWN`] after the first failure, doubling with each
/// further one up to [`MAX_COOLDOWN`]. A success forgets the session.
#[derive(Debug, Default)]
pub(crate) struct Cooldown {
    /// Failures in a row and the time (Unix seconds) the session may be tried again.
    failing: HashMap<SessionKey, (u32, i64)>,
}

impl Cooldown {
    /// Whether `key` may be tried at `now`.
    pub(crate) fn may_try(&self, key: &SessionKey, now: i64) -> bool {
        self.failing.get(key).is_none_or(|&(_, until)| now >= until)
    }

    /// `key` failed at `now`.
    pub(crate) fn failed(&mut self, key: &SessionKey, now: i64) {
        let failures = self
            .failing
            .get(key)
            .map_or(0, |&(n, _)| n)
            .saturating_add(1);
        // 2^5 hours is past the cap already; a smaller shift cannot overflow.
        let wait = (FIRST_COOLDOWN << (failures - 1).min(5)).min(MAX_COOLDOWN);
        self.failing
            .insert(key.clone(), (failures, now.saturating_add(wait)));
    }

    /// `key` was enriched.
    pub(crate) fn succeeded(&mut self, key: &SessionKey) {
        self.failing.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound_core::model::Harness;

    const T: i64 = 1_000_000;
    const HOUR: i64 = 3600;

    fn key(id: &str) -> SessionKey {
        SessionKey {
            harness: Harness::Claude,
            id: id.to_string(),
        }
    }

    #[test]
    fn due_needs_a_change_and_ten_minutes() {
        assert!(!enrich_due(None, None, T));
        assert!(!enrich_due(None, Some(T - 3 * HOUR), T));
        assert!(enrich_due(Some(T), None, T));
        assert!(enrich_due(Some(T), Some(T - ENRICH_EVERY), T));
        assert!(!enrich_due(Some(T), Some(T - ENRICH_EVERY + 1), T));
        assert!(!enrich_due(Some(T), Some(T), T));
        // A clock that went backwards never makes a pass due early.
        assert!(!enrich_due(Some(T), Some(T + 100), T));
        assert!(!enrich_due(Some(i64::MIN), Some(i64::MAX), i64::MIN));
    }

    #[test]
    fn schedule_runs_after_changes_at_most_every_ten_minutes() {
        let mut s = EnrichSchedule::new(600);
        assert!(!s.due(T));
        s.changed(T);
        assert!(s.due(T));
        s.passed(T);
        // Sessions changed at T are not quiet yet: the change stays pending,
        // but the next pass waits ten minutes.
        assert!(!s.due(T + 60));
        assert!(s.due(T + ENRICH_EVERY));
        s.passed(T + ENRICH_EVERY);
        // That pass came quiet_secs after the change and covered it.
        assert!(!s.due(T + 3 * ENRICH_EVERY));
        // A later change makes the next pass due once ten minutes have passed.
        s.changed(T + ENRICH_EVERY + 30);
        assert!(!s.due(T + ENRICH_EVERY + 60));
        assert!(s.due(T + 2 * ENRICH_EVERY));
    }

    #[test]
    fn schedule_with_no_quiet_period_covers_a_change_at_once() {
        let mut s = EnrichSchedule::new(0);
        s.changed(T);
        s.passed(T);
        assert!(!s.due(T + 10 * ENRICH_EVERY));
    }

    #[test]
    fn cooldown_doubles_up_to_a_day_and_clears_on_success() {
        let mut c = Cooldown::default();
        let k = key("a");
        let other = key("b");
        assert!(c.may_try(&k, T));

        c.failed(&k, T);
        assert!(!c.may_try(&k, T + HOUR - 1));
        assert!(c.may_try(&k, T + HOUR));
        assert!(c.may_try(&other, T));

        c.failed(&k, T + HOUR);
        assert!(!c.may_try(&k, T + 3 * HOUR - 1));
        assert!(c.may_try(&k, T + 3 * HOUR));

        c.failed(&k, T);
        assert!(!c.may_try(&k, T + 4 * HOUR - 1));
        assert!(c.may_try(&k, T + 4 * HOUR));

        for _ in 0..40 {
            c.failed(&k, T);
        }
        assert!(!c.may_try(&k, T + 24 * HOUR - 1));
        assert!(c.may_try(&k, T + 24 * HOUR));

        c.succeeded(&k);
        assert!(c.may_try(&k, T));
        // After a success the next failure starts again at an hour.
        c.failed(&k, T);
        assert!(c.may_try(&k, T + HOUR));
    }
}
