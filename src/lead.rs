//! Fenced leader lease for the queen armada.
//!
//! Several queens run at once; exactly one may allocate and mint JIT runners.
//! The leader holds a lease object, `{holder, epoch, expires_at}`, in a store
//! every queen can reach. The store is the only authority: a write succeeds
//! only if the object's generation still matches what the writer last read
//! (compare-and-swap), so two queens that race for the same expired lease
//! cannot both win. The trait [`LeaseStore`] mirrors an object store's
//! generation-match precondition (GCS `ifGenerationMatch`, S3 conditional
//! writes) without binding to either; this module carries no HTTP client.
//!
//! [`Generation`] is an opaque string. GCS generations are integers, but S3
//! compatible stores condition on an ETag, so a string carries both and the
//! module never does arithmetic on it.
//!
//! Decision rules ([`decide`]), evaluated against the lease as just read:
//!
//! * No lease, or `now >= expires_at` (expiry is inclusive): **acquire** with
//!   `epoch = previous epoch + 1`, guarded by the generation just read (or
//!   `Absent` when there is no object).
//! * We hold it and `now - renewed_at >= renew_secs`: **renew**, same epoch.
//! * We hold it and no renewal is due: **hold**; nothing is written.
//! * Someone else holds an unexpired lease: **follow**; a follower never writes.
//! * Stale reads: a record whose epoch is lower than one this queen has already
//!   seen is never followed or acted on. The epoch only moves forward, so such
//!   a record is an out-of-date read; [`decide`] returns `Hold` and
//!   [`Leader::step`] keeps its previous role (which still lapses when our own
//!   lease expires).
//!
//! [`Leader::step`] applies a decision. It never retries in a loop: on
//! `PreconditionFailed` it re-reads once and reports the role that results; on
//! `Unavailable` it keeps the previous role only while our own lease is
//! unexpired by the local clock (`now < expires_at`), and otherwise steps down.
//!
//! The epoch is the fencing token ([`fence_ok`], [`runner_name`]). Slice 2
//! wires this into the queen; nothing calls it yet.

use serde::{Deserialize, Serialize};

use crate::wake::BoxFuture;

/// The lease object as stored (JSON on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub holder: String,
    pub epoch: u64,
    /// Unix seconds. The lease is expired when `now >= expires_at`.
    pub expires_at: i64,
}

/// Opaque version of the lease object, as issued by the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation(pub String);

/// What a write requires of the object it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// Create only if no object exists.
    Absent,
    /// Replace only if the object still has this generation.
    Generation(Generation),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseStoreError {
    #[error("lease precondition failed")]
    PreconditionFailed,
    #[error("lease store unavailable: {0}")]
    Unavailable(String),
}

/// Conditional-write storage for the single lease object.
pub trait LeaseStore: Send + Sync {
    fn read(&self) -> BoxFuture<'_, Result<Option<(LeaseRecord, Generation)>, LeaseStoreError>>;

    fn write<'a>(
        &'a self,
        record: &'a LeaseRecord,
        expected: Precondition,
    ) -> BoxFuture<'a, Result<Generation, LeaseStoreError>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadSettings {
    pub ttl_secs: u64,
    pub renew_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeadError {
    #[error("lease ttl_secs and renew_secs must be greater than zero")]
    Zero,
    #[error("lease renew_secs ({renew}) must be less than ttl_secs ({ttl})")]
    RenewNotBeforeTtl { renew: u64, ttl: u64 },
    #[error("lease holder name must not be empty")]
    EmptyName,
}

impl LeadSettings {
    pub fn validate(&self) -> Result<(), LeadError> {
        if self.ttl_secs == 0 || self.renew_secs == 0 {
            return Err(LeadError::Zero);
        }
        if self.renew_secs >= self.ttl_secs {
            return Err(LeadError::RenewNotBeforeTtl {
                renew: self.renew_secs,
                ttl: self.ttl_secs,
            });
        }
        Ok(())
    }

    fn ttl(&self) -> i64 {
        i64::try_from(self.ttl_secs).unwrap_or(i64::MAX)
    }

    fn renew(&self) -> i64 {
        i64::try_from(self.renew_secs).unwrap_or(i64::MAX)
    }
}

/// What this queen believes it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub epoch: u64,
    pub generation: Generation,
    /// Time of the write that created or last renewed the lease.
    pub renewed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Acquire {
        record: LeaseRecord,
        precondition: Precondition,
    },
    Renew {
        record: LeaseRecord,
        precondition: Precondition,
    },
    Follow {
        leader: String,
        epoch: u64,
    },
    Hold,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Leader { epoch: u64 },
    Follower { leader: Option<String>, epoch: u64 },
}

impl Role {
    pub fn is_leader(&self) -> bool {
        matches!(self, Role::Leader { .. })
    }
}

/// Pure lease decision; see the module docs for the rules.
pub fn decide(
    me: &str,
    now: i64,
    settings: &LeadSettings,
    current: Option<&(LeaseRecord, Generation)>,
    held: Option<&Held>,
) -> Decision {
    let expires = now.saturating_add(settings.ttl());
    let Some((rec, generation)) = current else {
        return Decision::Acquire {
            record: LeaseRecord {
                holder: me.to_string(),
                epoch: held.map_or(0, |h| h.epoch).saturating_add(1),
                expires_at: expires,
            },
            precondition: Precondition::Absent,
        };
    };
    if held.is_some_and(|h| rec.epoch < h.epoch) {
        return Decision::Hold;
    }
    if now >= rec.expires_at {
        return Decision::Acquire {
            record: LeaseRecord {
                holder: me.to_string(),
                epoch: rec.epoch.saturating_add(1),
                expires_at: expires,
            },
            precondition: Precondition::Generation(generation.clone()),
        };
    }
    if rec.holder != me {
        return Decision::Follow {
            leader: rec.holder.clone(),
            epoch: rec.epoch,
        };
    }
    // Our name holds an unexpired lease. Without `held` (restart) adopt it.
    let renewed_at = held.map_or(rec.expires_at.saturating_sub(settings.ttl()), |h| {
        h.renewed_at
    });
    if now.saturating_sub(renewed_at) >= settings.renew() {
        Decision::Renew {
            record: LeaseRecord {
                holder: me.to_string(),
                epoch: rec.epoch,
                expires_at: expires,
            },
            precondition: Precondition::Generation(generation.clone()),
        }
    } else {
        Decision::Hold
    }
}

/// A grant whose epoch is lower than the highest seen is rejected.
pub fn fence_ok(seen_epoch: u64, grant_epoch: u64) -> bool {
    grant_epoch >= seen_epoch
}

/// `<prefix><drone>-e<epoch>-<lease_id>`.
pub fn runner_name(prefix: &str, drone: &str, epoch: u64, lease_id: &str) -> String {
    format!("{prefix}{drone}-e{epoch}-{lease_id}")
}

/// The epoch in a runner name. Lease ids contain no `-`, so the epoch is the
/// `e<digits>` segment just before the last one. The epoch-less format
/// `<prefix><drone>-<lease_id>` returns `None` (slice 2 treats it as epoch 0).
/// A legacy name whose drone part ends in `-e<digits>` is indistinguishable
/// from an epoch segment.
pub fn parse_runner_epoch(name: &str) -> Option<u64> {
    let (head, lease) = name.rsplit_once('-')?;
    if lease.is_empty() {
        return None;
    }
    let (rest, seg) = head.rsplit_once('-')?;
    if rest.is_empty() {
        return None;
    }
    let digits = seg.strip_prefix('e')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One queen's view of the lease. Drive it with [`Leader::step`] on a timer
/// well inside `renew_secs`.
#[derive(Debug)]
pub struct Leader {
    me: String,
    settings: LeadSettings,
    held: Option<Held>,
    seen_epoch: u64,
    role: Role,
}

impl Leader {
    pub fn new(me: impl Into<String>, settings: LeadSettings) -> Result<Self, LeadError> {
        let me = me.into();
        if me.is_empty() {
            return Err(LeadError::EmptyName);
        }
        settings.validate()?;
        Ok(Self {
            me,
            settings,
            held: None,
            seen_epoch: 0,
            role: Role::Follower {
                leader: None,
                epoch: 0,
            },
        })
    }

    pub fn role(&self) -> &Role {
        &self.role
    }

    /// Highest epoch this queen has seen; the floor for [`fence_ok`].
    pub fn seen_epoch(&self) -> u64 {
        self.seen_epoch
    }

    fn held_expires_at(&self) -> Option<i64> {
        self.held
            .as_ref()
            .map(|h| h.renewed_at.saturating_add(self.settings.ttl()))
    }

    fn follow(&mut self, leader: Option<String>, epoch: u64) -> Role {
        self.held = None;
        self.seen_epoch = self.seen_epoch.max(epoch);
        self.role = Role::Follower {
            leader,
            epoch: self.seen_epoch,
        };
        self.role.clone()
    }

    fn lead(&mut self, epoch: u64, generation: Generation, renewed_at: i64) -> Role {
        self.seen_epoch = self.seen_epoch.max(epoch);
        self.held = Some(Held {
            epoch,
            generation,
            renewed_at,
        });
        self.role = Role::Leader { epoch };
        self.role.clone()
    }

    /// Keep the previous role only while our own lease is unexpired.
    fn keep_or_step_down(&mut self, now: i64) -> Role {
        match (&self.role, self.held_expires_at()) {
            (Role::Leader { .. }, Some(exp)) if now < exp => self.role.clone(),
            (Role::Leader { .. }, _) => self.follow(None, 0),
            _ => self.role.clone(),
        }
    }

    /// Adopt whatever the store says without writing.
    fn adopt(&mut self, now: i64, current: Option<(LeaseRecord, Generation)>) -> Role {
        match current {
            Some((rec, generation)) if rec.epoch >= self.seen_epoch && now < rec.expires_at => {
                if rec.holder == self.me {
                    let renewed_at = rec.expires_at.saturating_sub(self.settings.ttl());
                    self.lead(rec.epoch, generation, renewed_at)
                } else {
                    self.follow(Some(rec.holder), rec.epoch)
                }
            }
            Some((rec, _)) if rec.epoch >= self.seen_epoch => self.follow(None, rec.epoch),
            Some(_) => self.keep_or_step_down(now),
            None => self.follow(None, 0),
        }
    }

    pub async fn step(&mut self, store: &dyn LeaseStore, now: i64) -> Role {
        let Ok(current) = store.read().await else {
            return self.keep_or_step_down(now);
        };
        if current
            .as_ref()
            .is_some_and(|(r, _)| r.epoch < self.seen_epoch)
        {
            return self.keep_or_step_down(now);
        }
        match decide(
            &self.me,
            now,
            &self.settings,
            current.as_ref(),
            self.held.as_ref(),
        ) {
            Decision::Hold => self.keep_or_step_down(now),
            Decision::Follow { leader, epoch } => self.follow(Some(leader), epoch),
            Decision::Acquire {
                mut record,
                precondition,
            } => {
                record.epoch = record.epoch.max(self.seen_epoch.saturating_add(1));
                self.write(store, now, record, precondition).await
            }
            Decision::Renew {
                record,
                precondition,
            } => self.write(store, now, record, precondition).await,
        }
    }

    async fn write(
        &mut self,
        store: &dyn LeaseStore,
        now: i64,
        record: LeaseRecord,
        precondition: Precondition,
    ) -> Role {
        match store.write(&record, precondition).await {
            Ok(generation) => self.lead(record.epoch, generation, now),
            Err(LeaseStoreError::PreconditionFailed) => match store.read().await {
                Ok(current) => self.adopt(now, current),
                Err(_) => self.keep_or_step_down(now),
            },
            Err(LeaseStoreError::Unavailable(_)) => self.keep_or_step_down(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct State {
        object: Option<(LeaseRecord, u64)>,
        counter: u64,
        writes: Vec<(LeaseRecord, Precondition)>,
        fail_next_write: Option<LeaseStoreError>,
        /// Another queen's write that lands just before the next write is checked.
        interleave: Option<LeaseRecord>,
        down: bool,
    }

    #[derive(Default)]
    struct FakeStore(Mutex<State>);

    impl FakeStore {
        fn fail_next_write_with(&self, e: LeaseStoreError) {
            self.0.lock().unwrap().fail_next_write = Some(e);
        }
        fn interleave_before_next_write(&self, r: LeaseRecord) {
            self.0.lock().unwrap().interleave = Some(r);
        }
        fn set_down(&self, down: bool) {
            self.0.lock().unwrap().down = down;
        }
        fn writes(&self) -> Vec<(LeaseRecord, Precondition)> {
            self.0.lock().unwrap().writes.clone()
        }
        fn record(&self) -> Option<LeaseRecord> {
            self.0.lock().unwrap().object.as_ref().map(|o| o.0.clone())
        }
    }

    impl LeaseStore for FakeStore {
        fn read(
            &self,
        ) -> BoxFuture<'_, Result<Option<(LeaseRecord, Generation)>, LeaseStoreError>> {
            Box::pin(async move {
                let s = self.0.lock().unwrap();
                if s.down {
                    return Err(LeaseStoreError::Unavailable("down".into()));
                }
                Ok(s.object
                    .as_ref()
                    .map(|(r, g)| (r.clone(), Generation(g.to_string()))))
            })
        }

        fn write<'a>(
            &'a self,
            record: &'a LeaseRecord,
            expected: Precondition,
        ) -> BoxFuture<'a, Result<Generation, LeaseStoreError>> {
            Box::pin(async move {
                let mut s = self.0.lock().unwrap();
                s.writes.push((record.clone(), expected.clone()));
                if s.down {
                    return Err(LeaseStoreError::Unavailable("down".into()));
                }
                if let Some(e) = s.fail_next_write.take() {
                    return Err(e);
                }
                if let Some(other) = s.interleave.take() {
                    s.counter += 1;
                    let g = s.counter;
                    s.object = Some((other, g));
                }
                let ok = match (&s.object, &expected) {
                    (None, Precondition::Absent) => true,
                    (Some((_, g)), Precondition::Generation(want)) => g.to_string() == want.0,
                    _ => false,
                };
                if !ok {
                    return Err(LeaseStoreError::PreconditionFailed);
                }
                s.counter += 1;
                let g = s.counter;
                s.object = Some((record.clone(), g));
                Ok(Generation(g.to_string()))
            })
        }
    }

    fn settings() -> LeadSettings {
        LeadSettings {
            ttl_secs: 30,
            renew_secs: 10,
        }
    }

    fn queen(name: &str) -> Leader {
        Leader::new(name, settings()).unwrap()
    }

    fn generation(s: &str) -> Generation {
        Generation(s.to_string())
    }

    fn lease(holder: &str, epoch: u64, expires_at: i64) -> LeaseRecord {
        LeaseRecord {
            holder: holder.into(),
            epoch,
            expires_at,
        }
    }

    fn follower(leader: &str, epoch: u64) -> Role {
        Role::Follower {
            leader: Some(leader.into()),
            epoch,
        }
    }

    #[tokio::test]
    async fn two_queens_one_leader() {
        let store = FakeStore::default();
        let mut a = queen("a");
        let mut b = queen("b");
        assert_eq!(a.step(&store, 100).await, Role::Leader { epoch: 1 });
        assert_eq!(b.step(&store, 100).await, follower("a", 1));
        assert_eq!(store.writes().len(), 1);
    }

    #[tokio::test]
    async fn racing_acquire_loser_follows_winner() {
        let store = FakeStore::default();
        let mut a = queen("a");
        // b's create lands between a's read and a's write.
        store.interleave_before_next_write(lease("b", 1, 130));
        assert_eq!(a.step(&store, 100).await, follower("b", 1));
        assert_eq!(store.record().unwrap().holder, "b");
    }

    #[tokio::test]
    async fn leader_renews_before_ttl_and_keeps_epoch() {
        let store = FakeStore::default();
        let mut a = queen("a");
        let mut b = queen("b");
        a.step(&store, 100).await;
        b.step(&store, 100).await;
        let mut t = 100;
        for _ in 0..6 {
            t += 5;
            assert_eq!(a.step(&store, t).await, Role::Leader { epoch: 1 });
            assert_eq!(b.step(&store, t).await, follower("a", 1));
        }
        let rec = store.record().unwrap();
        assert_eq!((rec.holder.as_str(), rec.epoch), ("a", 1));
        assert!(rec.expires_at > t);
        // Only a wrote: acquire at 100, renewals at 110, 120, 130.
        assert!(store.writes().iter().all(|(r, _)| r.holder == "a"));
        assert_eq!(store.writes().len(), 4);
    }

    #[tokio::test]
    async fn takeover_after_expiry_and_no_split_brain() {
        let store = FakeStore::default();
        let mut a = queen("a");
        let mut b = queen("b");
        a.step(&store, 100).await; // expires at 130
        b.step(&store, 100).await;
        // a is paused. b follows until expiry, which is inclusive.
        assert!(!b.step(&store, 129).await.is_leader());
        assert_eq!(b.step(&store, 130).await, Role::Leader { epoch: 2 });
        // a wakes up and sees the new holder.
        assert_eq!(a.step(&store, 131).await, follower("b", 2));
        for now in 131..300 {
            let ra = a.step(&store, now).await;
            let rb = b.step(&store, now).await;
            assert!(!(ra.is_leader() && rb.is_leader()), "split brain at {now}");
        }
    }

    #[tokio::test]
    async fn old_leader_renew_loses_cas_and_follows() {
        let store = FakeStore::default();
        let mut a = queen("a");
        a.step(&store, 100).await; // gen 1, expires 130
        // b takes over between a's read and a's renew write.
        store.interleave_before_next_write(lease("b", 2, 200));
        assert_eq!(a.step(&store, 110).await, follower("b", 2));
        assert_eq!(a.seen_epoch(), 2);
        // Exactly one renew was attempted, no retry loop.
        assert_eq!(store.writes().len(), 2);
        assert_eq!(store.record().unwrap().holder, "b");
    }

    #[tokio::test]
    async fn precondition_failed_with_us_still_holding_keeps_leading() {
        let store = FakeStore::default();
        let mut a = queen("a");
        a.step(&store, 100).await;
        store.fail_next_write_with(LeaseStoreError::PreconditionFailed);
        assert_eq!(a.step(&store, 110).await, Role::Leader { epoch: 1 });
    }

    #[tokio::test]
    async fn follower_never_writes_while_lease_unexpired() {
        let store = FakeStore::default();
        let mut a = queen("a");
        let mut b = queen("b");
        a.step(&store, 100).await;
        let before = store.writes().len();
        for t in 100..130 {
            b.step(&store, t).await;
        }
        assert_eq!(store.writes().len(), before);
    }

    #[test]
    fn stale_epoch_is_fenced() {
        assert!(!fence_ok(5, 4));
        assert!(fence_ok(5, 5));
        assert!(fence_ok(5, 6));
        let held = Held {
            epoch: 5,
            generation: generation("9"),
            renewed_at: 100,
        };
        let stale = (lease("other", 4, 10_000), generation("3"));
        let d = decide("me", 101, &settings(), Some(&stale), Some(&held));
        assert_eq!(d, Decision::Hold);
    }

    #[tokio::test]
    async fn stale_read_is_not_followed_by_step() {
        let store = FakeStore::default();
        let mut a = queen("a");
        let mut b = queen("b");
        a.step(&store, 100).await;
        b.step(&store, 140).await; // epoch 2
        assert_eq!(b.seen_epoch(), 2);
        // The store serves an older epoch (stale replica): b keeps its role.
        store.0.lock().unwrap().object = Some((lease("a", 1, 10_000), 77));
        assert_eq!(b.step(&store, 141).await, Role::Leader { epoch: 2 });
        // A follower that has seen epoch 2 does not follow epoch 1 either.
        let mut c = queen("c");
        store.0.lock().unwrap().object = Some((lease("b", 2, 10_000), 78));
        assert_eq!(c.step(&store, 142).await, follower("b", 2));
        store.0.lock().unwrap().object = Some((lease("a", 1, 10_000), 79));
        assert_eq!(c.step(&store, 143).await, follower("b", 2));
    }

    #[tokio::test]
    async fn store_unavailable_leader_keeps_until_own_expiry() {
        let store = FakeStore::default();
        let mut a = queen("a");
        a.step(&store, 100).await; // expires 130
        store.set_down(true);
        assert_eq!(a.step(&store, 115).await, Role::Leader { epoch: 1 });
        assert_eq!(a.step(&store, 129).await, Role::Leader { epoch: 1 });
        assert!(!a.step(&store, 130).await.is_leader());
        assert_eq!(a.role().clone(), a.step(&store, 131).await);
        assert!(matches!(a.role(), Role::Follower { leader: None, .. }));
    }

    #[tokio::test]
    async fn failed_renew_write_does_not_extend_local_expiry() {
        let store = FakeStore::default();
        let mut a = queen("a");
        a.step(&store, 100).await;
        store.fail_next_write_with(LeaseStoreError::Unavailable("x".into()));
        assert_eq!(a.step(&store, 110).await, Role::Leader { epoch: 1 });
        store.set_down(true);
        assert!(!a.step(&store, 130).await.is_leader());
    }

    #[tokio::test]
    async fn store_unavailable_follower_stays_follower() {
        let store = FakeStore::default();
        let mut a = queen("a");
        let mut b = queen("b");
        a.step(&store, 100).await;
        b.step(&store, 100).await;
        store.set_down(true);
        assert_eq!(b.step(&store, 200).await, follower("a", 1));
        assert_eq!(store.writes().len(), 1);
    }

    #[test]
    fn decide_rules() {
        let s = settings();
        let rec = |h: &str, e: u64, x: i64| (lease(h, e, x), generation("g"));
        let held = Held {
            epoch: 3,
            generation: generation("g"),
            renewed_at: 100,
        };
        assert!(matches!(
            decide("me", 100, &s, None, None),
            Decision::Acquire { record, precondition: Precondition::Absent } if record.epoch == 1
        ));
        assert!(matches!(
            decide("me", 130, &s, Some(&rec("x", 7, 130)), None),
            Decision::Acquire { record, precondition: Precondition::Generation(_) }
                if record.epoch == 8 && record.expires_at == 160
        ));
        assert_eq!(
            decide("me", 105, &s, Some(&rec("me", 3, 130)), Some(&held)),
            Decision::Hold
        );
        assert!(matches!(
            decide("me", 110, &s, Some(&rec("me", 3, 130)), Some(&held)),
            Decision::Renew { record, .. } if record.epoch == 3 && record.expires_at == 140
        ));
        assert_eq!(
            decide("me", 105, &s, Some(&rec("x", 3, 130)), Some(&held)),
            Decision::Follow {
                leader: "x".into(),
                epoch: 3
            }
        );
    }

    #[test]
    fn lease_record_json_round_trip() {
        let r = lease("a", 3, 42);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"holder":"a","epoch":3,"expires_at":42}"#);
        assert_eq!(serde_json::from_str::<LeaseRecord>(&json).unwrap(), r);
    }

    #[test]
    fn runner_names_round_trip() {
        let n = runner_name("borg-", "host-a", 12, "abc123");
        assert_eq!(n, "borg-host-a-e12-abc123");
        assert_eq!(parse_runner_epoch(&n), Some(12));
        assert_eq!(parse_runner_epoch(&runner_name("", "d", 0, "ff")), Some(0));
        assert_eq!(parse_runner_epoch("borg-host-a-abc123"), None);
        assert_eq!(parse_runner_epoch("nodash"), None);
        assert_eq!(parse_runner_epoch("borg-e-abc"), None);
    }

    #[test]
    fn settings_validation() {
        let check = |t, r| {
            LeadSettings {
                ttl_secs: t,
                renew_secs: r,
            }
            .validate()
        };
        assert!(check(30, 10).is_ok());
        assert!(check(30, 30).is_err());
        assert!(check(30, 31).is_err());
        assert_eq!(check(0, 0), Err(LeadError::Zero));
        assert_eq!(check(30, 0), Err(LeadError::Zero));
        assert_eq!(check(0, 1), Err(LeadError::Zero));
        let bad = LeadSettings {
            ttl_secs: 5,
            renew_secs: 5,
        };
        assert!(Leader::new("a", bad).is_err());
        assert!(Leader::new("", settings()).is_err());
    }
}
