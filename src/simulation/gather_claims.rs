//! Soft, auto-expiring gather claims on resource tiles.
//!
//! When an HTN dispatcher picks a tile to gather from, it adds an entry here
//! so other agents biased toward "least-claimed" via
//! `SharedKnowledge::nearest_in_tier_set`'s `claim_penalty` parameter pick a
//! different cluster instead of racing. Mirrors the `StorageReservations`
//! pattern: mutex-wrapped so the parallel resolver / HTN dispatchers can read
//! and write concurrently. Auto-expire (by tick) prevents zombie claims when
//! an agent is preempted before it can release.
//!
//! Claims are an *advisory* layer — the gather executor still validates the
//! tile on arrival and reports depletion to `SharedKnowledge`. The claim
//! exists only to spread agents across multiple known clusters when the
//! faction has more workers than nearby resources.

use crate::simulation::memory::MemoryKind;
use crate::simulation::SimClock;
use ahash::AHashMap;
use bevy::prelude::*;
use std::sync::Mutex;

/// P4 (cluster-slot mutex): how many concurrent gatherers a single
/// `ResourceCluster` admits before it's considered fully spoken for.
/// Tuned conservative — three reps per cluster on average gives each
/// claimant a distinct rep before saturation, after which the
/// dispatcher should pick a different cluster. Above this threshold,
/// `pressure_across_tiles` returns >= MAX and `cluster_is_saturated`
/// flips true.
pub const MAX_PARALLEL_GATHERERS_PER_CLUSTER: i32 = 3;

/// One outstanding claim on a `(tile, kind)` pair.
#[derive(Clone, Copy, Debug)]
pub struct GatherClaim {
    pub claimant: Entity,
    pub kind: MemoryKind,
    pub expires_tick: u64,
}

#[derive(Resource, Default)]
pub struct GatherClaims {
    inner: Mutex<AHashMap<(i32, i32), Vec<GatherClaim>>>,
}

impl GatherClaims {
    /// Stake a claim on `tile` for `kind` until `expires_tick`. Idempotent
    /// per `(claimant, tile, kind)` — re-staking just refreshes the expiry.
    pub fn add(&self, tile: (i32, i32), kind: MemoryKind, claimant: Entity, expires_tick: u64) {
        let mut m = self.inner.lock().unwrap();
        let slot = m.entry(tile).or_default();
        for c in slot.iter_mut() {
            if c.claimant == claimant && c.kind == kind {
                c.expires_tick = expires_tick;
                return;
            }
        }
        slot.push(GatherClaim {
            claimant,
            kind,
            expires_tick,
        });
    }

    /// Drop a claim. Safe to call on a tile that was never claimed.
    pub fn release(&self, tile: (i32, i32), kind: MemoryKind, claimant: Entity) {
        let mut m = self.inner.lock().unwrap();
        let Some(slot) = m.get_mut(&tile) else { return };
        slot.retain(|c| !(c.claimant == claimant && c.kind == kind));
        if slot.is_empty() {
            m.remove(&tile);
        }
    }

    /// Count active (not-expired) claims on `tile` not held by `viewer`.
    /// HTN dispatchers use this as the `claim_penalty` for nearest-cluster
    /// scoring — heavily-claimed tiles are pushed back so workers naturally
    /// fan out across known clusters.
    pub fn pressure(&self, tile: (i32, i32), now: u64, viewer: Entity) -> i32 {
        let m = self.inner.lock().unwrap();
        let Some(slot) = m.get(&tile) else { return 0 };
        slot.iter()
            .filter(|c| c.expires_tick >= now && c.claimant != viewer)
            .count() as i32
    }

    /// P4 (cluster-slot mutex): count active claims across all `rep_tiles`
    /// of a single cluster, excluding `viewer`. Used by dispatchers to
    /// gate "is this cluster fully spoken for already?" — when the count
    /// reaches `MAX_PARALLEL_GATHERERS_PER_CLUSTER`, the cluster is
    /// effectively claimed and the dispatcher should pick a different
    /// one. Complements the per-rep `pressure()` (P6c) which spreads
    /// agents *within* a cluster.
    pub fn pressure_across_tiles(
        &self,
        rep_tiles: impl IntoIterator<Item = (i32, i32)>,
        now: u64,
        viewer: Entity,
    ) -> i32 {
        let m = self.inner.lock().unwrap();
        let mut total = 0i32;
        for tile in rep_tiles {
            if let Some(slot) = m.get(&tile) {
                total += slot
                    .iter()
                    .filter(|c| c.expires_tick >= now && c.claimant != viewer)
                    .count() as i32;
            }
        }
        total
    }

    /// Sweep expired entries. Called by `gather_claim_expiry_system`.
    pub fn sweep_expired(&self, now: u64) {
        let mut m = self.inner.lock().unwrap();
        m.retain(|_, slot| {
            slot.retain(|c| c.expires_tick >= now);
            !slot.is_empty()
        });
    }

    /// Total live entries — for inspector / debug.
    pub fn total(&self) -> usize {
        self.inner.lock().unwrap().values().map(|v| v.len()).sum()
    }

    /// P4 (cluster-slot mutex): true when a cluster's rep tiles already
    /// hold `MAX_PARALLEL_GATHERERS_PER_CLUSTER` distinct claims (after
    /// excluding `viewer`). Dispatchers use this to skip a cluster
    /// outright when scoring — saturated clusters lose to less-pressured
    /// ones even when they're physically closer.
    pub fn cluster_is_saturated(
        &self,
        rep_tiles: impl IntoIterator<Item = (i32, i32)>,
        now: u64,
        viewer: Entity,
    ) -> bool {
        self.pressure_across_tiles(rep_tiles, now, viewer)
            >= MAX_PARALLEL_GATHERERS_PER_CLUSTER
    }
}

/// Release any gather claim held on `ai.active_gather_claim`. Mirrors
/// `release_reservation` for storage. Safe to call from any teardown path.
pub fn release_gather_claim(
    claims: &GatherClaims,
    ai: &mut crate::simulation::person::PersonAI,
    actor: Entity,
) {
    if let Some((tile, kind)) = ai.active_gather_claim {
        claims.release(tile, kind, actor);
    }
    ai.active_gather_claim = None;
}

/// Periodic expiry sweep. Cheap; runs every `EXPIRY_CADENCE` ticks.
pub fn gather_claim_expiry_system(clock: Res<SimClock>, claims: Res<GatherClaims>) {
    const EXPIRY_CADENCE: u64 = 150; // 7.5s game-time @ 20Hz
    if clock.tick % EXPIRY_CADENCE != 0 {
        return;
    }
    claims.sweep_expired(clock.tick);
}

/// Suggested expiry budget for a fresh claim: a generous estimate of the
/// time it should take to walk to `target` from `from` and complete a gather
/// task. Centralised so dispatchers don't drift.
pub fn suggested_expiry(now: u64, from: (i32, i32), target: (i32, i32)) -> u64 {
    let dist = (from.0 - target.0).abs().max((from.1 - target.1).abs()) as u64;
    now + dist * 4 + 200
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(id: u32) -> Entity {
        Entity::from_raw(id)
    }

    #[test]
    fn add_and_release_round_trip() {
        let c = GatherClaims::default();
        c.add((1, 1), MemoryKind::AnyEdible, ent(1), 100);
        assert_eq!(c.total(), 1);
        c.release((1, 1), MemoryKind::AnyEdible, ent(1));
        assert_eq!(c.total(), 0);
    }

    #[test]
    fn re_add_refreshes_expiry() {
        let c = GatherClaims::default();
        c.add((1, 1), MemoryKind::AnyEdible, ent(1), 100);
        c.add((1, 1), MemoryKind::AnyEdible, ent(1), 500);
        assert_eq!(c.total(), 1);
        c.sweep_expired(200);
        assert_eq!(c.total(), 1, "sweep below new expiry should keep entry");
    }

    #[test]
    fn pressure_excludes_self() {
        let c = GatherClaims::default();
        c.add((1, 1), MemoryKind::AnyEdible, ent(1), 100);
        c.add((1, 1), MemoryKind::AnyEdible, ent(2), 100);
        assert_eq!(c.pressure((1, 1), 50, ent(1)), 1);
        assert_eq!(c.pressure((1, 1), 50, ent(2)), 1);
        assert_eq!(c.pressure((1, 1), 50, ent(3)), 2);
    }

    #[test]
    fn pressure_excludes_expired() {
        let c = GatherClaims::default();
        c.add((1, 1), MemoryKind::AnyEdible, ent(1), 100);
        c.add((1, 1), MemoryKind::AnyEdible, ent(2), 200);
        assert_eq!(c.pressure((1, 1), 150, ent(99)), 1);
    }

    #[test]
    fn sweep_drops_expired() {
        let c = GatherClaims::default();
        c.add((1, 1), MemoryKind::AnyEdible, ent(1), 100);
        c.add((2, 2), MemoryKind::AnyEdible, ent(2), 500);
        c.sweep_expired(200);
        assert_eq!(c.total(), 1);
    }

    /// P4 cluster-slot: pressure_across_tiles sums claims over multiple
    /// rep tiles, excluding the viewer.
    #[test]
    fn pressure_across_tiles_sums_per_rep() {
        let c = GatherClaims::default();
        c.add((0, 0), MemoryKind::AnyEdible, ent(1), 100);
        c.add((1, 0), MemoryKind::AnyEdible, ent(2), 100);
        c.add((2, 0), MemoryKind::AnyEdible, ent(3), 100);
        let reps = vec![(0, 0), (1, 0), (2, 0)];
        // Viewer = ent(99) (none of the claimants): sees all 3.
        assert_eq!(
            c.pressure_across_tiles(reps.iter().copied(), 50, ent(99)),
            3
        );
        // Viewer = ent(1): excluded from the count.
        assert_eq!(c.pressure_across_tiles(reps.iter().copied(), 50, ent(1)), 2);
    }

    /// P4 cluster-slot: cluster_is_saturated trips at MAX claims.
    #[test]
    fn cluster_is_saturated_at_max_parallel() {
        let c = GatherClaims::default();
        let reps = vec![(0, 0), (1, 0), (2, 0), (3, 0)];
        // Below the threshold.
        c.add((0, 0), MemoryKind::AnyEdible, ent(1), 100);
        c.add((1, 0), MemoryKind::AnyEdible, ent(2), 100);
        assert!(!c.cluster_is_saturated(reps.iter().copied(), 50, ent(99)));
        // At the threshold.
        c.add((2, 0), MemoryKind::AnyEdible, ent(3), 100);
        assert!(c.cluster_is_saturated(reps.iter().copied(), 50, ent(99)));
    }

    #[test]
    fn suggested_expiry_scales_with_distance() {
        let near = suggested_expiry(0, (0, 0), (5, 0));
        let far = suggested_expiry(0, (0, 0), (50, 0));
        assert!(far > near);
    }
}
