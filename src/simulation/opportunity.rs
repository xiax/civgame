use crate::collections::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::jobs::{JobBoard, JobId, JobKind};
use crate::simulation::medicine::Injury;
use crate::simulation::perf::{BackgroundWorkDiagnostics, PerfWorkBudget};
use crate::simulation::schedule::SimClock;
use crate::world::chunk::CHUNK_SIZE;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::terrain::TILE_SIZE;

/// Structured choices produced by institutions and broad cache builders.
/// Scorers read this surface instead of each one scanning raw ECS state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpportunityKind {
    PaidJob,
    MaterialDeficit,
    FoodSource,
    CareNeed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpportunityPayload {
    PaidJob {
        job_id: JobId,
        kind: JobKind,
        reward: f32,
        target: Option<ResourceId>,
    },
    MaterialDeficit {
        resource_id: ResourceId,
        deficit: u32,
    },
    FoodSource {
        qty: u32,
    },
    CareNeed {
        patient: Entity,
        severity: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opportunity {
    pub kind: OpportunityKind,
    pub tile: (i32, i32),
    pub faction_id: u32,
    pub payload: OpportunityPayload,
    pub expires_tick: u64,
}

/// Per-faction storage. Dirty-rebuild replaces one faction's bucket per
/// per-tick budget step without disturbing the others. Secondary maps
/// (`by_region_kind`) are derived lazily via the iterator — most consumers
/// hit `iter_kind_for_faction` (the hot path).
#[derive(Resource, Default, Debug)]
pub struct OpportunityIndex {
    per_faction: AHashMap<u32, Vec<Opportunity>>,
    pub rebuilt_at_tick: u64,
}

impl OpportunityIndex {
    /// Wipe everything (test/migration helper).
    pub fn clear(&mut self, now: u64) {
        self.per_faction.clear();
        self.rebuilt_at_tick = now;
    }

    /// Replace one faction's bucket. The hot-path mutator: per-tick partial
    /// rebuild drains the dirty set via this entry point.
    pub fn replace_for_faction(&mut self, faction_id: u32, entries: Vec<Opportunity>) {
        if entries.is_empty() {
            self.per_faction.remove(&faction_id);
        } else {
            self.per_faction.insert(faction_id, entries);
        }
    }

    /// Append a single opportunity for `faction_id`. Used by the rebuild
    /// helper to assemble each faction's bucket.
    pub fn push(&mut self, opportunity: Opportunity) {
        self.per_faction
            .entry(opportunity.faction_id)
            .or_default()
            .push(opportunity);
    }

    pub fn len(&self) -> usize {
        self.per_faction.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.per_faction.values().all(|v| v.is_empty())
    }

    pub fn iter_kind(&self, kind: OpportunityKind) -> impl Iterator<Item = &Opportunity> + '_ {
        self.per_faction
            .values()
            .flat_map(|v| v.iter())
            .filter(move |o| o.kind == kind)
    }

    pub fn iter_kind_for_faction(
        &self,
        faction_id: u32,
        kind: OpportunityKind,
    ) -> impl Iterator<Item = &Opportunity> + '_ {
        self.per_faction
            .get(&faction_id)
            .into_iter()
            .flat_map(|v| v.iter())
            .filter(move |o| o.kind == kind)
    }

    pub fn iter_kind_for_region(
        &self,
        tile: (i32, i32),
        kind: OpportunityKind,
    ) -> impl Iterator<Item = &Opportunity> + '_ {
        let region = region_key(tile);
        self.per_faction
            .values()
            .flat_map(|v| v.iter())
            .filter(move |o| o.kind == kind && region_key(o.tile) == region)
    }

    /// Drop expired entries from every faction bucket. Cheap: one filter
    /// pass per faction; runs every tick (not 20-tick-burst).
    pub fn evict_expired(&mut self, now: u64) {
        self.per_faction.retain(|_, entries| {
            entries.retain(|o| o.expires_tick > now);
            !entries.is_empty()
        });
    }
}

/// Rebuild scheduler for the opportunity index.
///
/// **This is a round-robin rebuilder, not an event-driven dirty queue.**
/// The rebuild system re-marks every faction each tick and the cursor +
/// per-tick `cap` drain it `cap` factions at a time, so every faction is
/// revisited within `ceil(N_factions / cap)` ticks. This is the correct
/// bulk mechanism here: the `FoodSource` / `MaterialDeficit` buckets derive
/// from `FactionStorage`, which `compute_faction_storage_system` rewrites
/// every Economy tick, so most buckets are genuinely stale most ticks —
/// there is no cheap "did this faction's storage projection change" signal
/// short of recomputing the bucket. With the usual ≤ `cap` materialised
/// factions every faction rebuilds every tick anyway; the cursor only
/// spreads work in large-faction (many-household, e.g. Market-preset) games.
///
/// The **priority lane** (`mark_priority`) is the one place producer marks
/// pay off: a faction with a freshly added / changed / cleared `Injury`
/// jumps ahead of the round-robin so its latency-sensitive `CareNeed`
/// surfaces within one tick instead of waiting up to `N/cap` ticks for the
/// cursor to reach it. It composes with — does not replace — the round
/// robin, and is a no-op when `N_factions ≤ cap`.
#[derive(Resource, Default, Debug)]
pub struct OpportunityDirty {
    /// Round-robin set: re-marked every tick by the rebuild system.
    pub factions: AHashSet<u32>,
    /// Priority set: factions with a latency-sensitive change (injury) that
    /// should be rebuilt ahead of the round-robin this tick.
    pub priority: AHashSet<u32>,
    /// Last tick we ran the daily-audit full-mark backstop.
    pub last_full_audit_tick: u64,
    /// Round-robin cursor over the sorted faction id space. Advanced by
    /// the drain step so we always make progress over the whole population.
    pub cursor: u32,
}

impl OpportunityDirty {
    /// Mark one faction's bucket for rebuild (round-robin lane).
    pub fn mark(&mut self, faction_id: u32) {
        self.factions.insert(faction_id);
    }

    /// Mark a faction for *priority* rebuild ahead of the round-robin —
    /// for latency-sensitive changes (injury add/change/clear → `CareNeed`).
    pub fn mark_priority(&mut self, faction_id: u32) {
        self.priority.insert(faction_id);
    }

    /// Mark every faction in `fids` for rebuild (round-robin lane).
    pub fn mark_all<I: IntoIterator<Item = u32>>(&mut self, fids: I) {
        for fid in fids {
            self.factions.insert(fid);
        }
    }

    /// Drain up to `cap` faction ids. The priority lane is served first;
    /// the remaining budget is filled from the round-robin set starting at
    /// the rolling `cursor` so a saturated set advances around the faction
    /// ring instead of starving the highest-id factions. The cursor is
    /// advanced only by the round-robin portion so priority picks (which may
    /// be small ids) don't rewind it.
    pub fn drain_up_to(&mut self, cap: usize) -> Vec<u32> {
        if cap == 0 {
            return Vec::new();
        }
        let mut taken: Vec<u32> = Vec::new();

        // Priority lane first.
        if !self.priority.is_empty() {
            let mut pr: Vec<u32> = self.priority.iter().copied().collect();
            pr.sort_unstable();
            for fid in pr.into_iter().take(cap) {
                self.priority.remove(&fid);
                self.factions.remove(&fid);
                taken.push(fid);
            }
        }

        // Fill remaining budget from the round-robin set near the cursor.
        let remaining = cap.saturating_sub(taken.len());
        if remaining > 0 && !self.factions.is_empty() {
            let mut snap: Vec<u32> = self.factions.iter().copied().collect();
            snap.sort_unstable();
            let pivot = snap.partition_point(|&f| f < self.cursor);
            snap.rotate_left(pivot);
            let mut last_rr: Option<u32> = None;
            for fid in snap.into_iter().take(remaining) {
                self.factions.remove(&fid);
                last_rr = Some(fid);
                if !taken.contains(&fid) {
                    taken.push(fid);
                }
            }
            // Advance cursor past the last round-robin faction drained.
            if let Some(last) = last_rr {
                self.cursor = last.saturating_add(1);
            }
        }

        taken
    }
}

fn region_key(tile: (i32, i32)) -> (i32, i32) {
    (
        tile.0.div_euclid(CHUNK_SIZE as i32),
        tile.1.div_euclid(CHUNK_SIZE as i32),
    )
}

/// Entry lifetime — long enough that a faction whose dirty mark missed the
/// per-tick drain still surfaces fresh entries within ~1 day. Daily full
/// audit re-marks every faction as a backstop.
pub const OPPORTUNITY_TTL: u64 = 40;

/// Per-tick: evict expired + drain up to `PerfWorkBudget::opportunity_rebuilds_per_tick`
/// dirty factions. Each drained faction has its bucket rebuilt from live
/// `JobBoard` + `FactionRegistry` + `Injury` state. No 20-tick burst.
///
/// Backstop: once per game-day, mark every faction dirty so a missed
/// producer-side dirty mark recovers within one day.
pub fn rebuild_opportunity_index_system(
    clock: Res<SimClock>,
    board: Res<JobBoard>,
    registry: Res<FactionRegistry>,
    budget: Res<PerfWorkBudget>,
    injured: Query<(Entity, &FactionMember, &Transform, Ref<Injury>)>,
    mut injury_cleared: RemovedComponents<Injury>,
    member_lookup: Query<&FactionMember>,
    mut index: ResMut<OpportunityIndex>,
    mut dirty: ResMut<OpportunityDirty>,
    mut bg: ResMut<BackgroundWorkDiagnostics>,
) {
    let now = clock.tick;
    let t_start = std::time::Instant::now();

    // Cheap every-tick expiry.
    index.evict_expired(now);

    // Round-robin lane: re-mark every faction each tick. The cursor + cap
    // drain bound the work regardless; storage-derived buckets churn every
    // tick so this is the correct bulk mechanism (see `OpportunityDirty`).
    dirty.mark_all(registry.factions.keys().copied());

    // Priority lane: a freshly added / changed `Injury` (and a cleared one —
    // healed agent) is latency-sensitive for `CareNeed`, so jump its faction
    // ahead of the round-robin. No-op when `N_factions ≤ cap` (everything
    // drains every tick anyway); the win is large-faction Market games.
    for (_e, member, _t, injury) in injured.iter() {
        if injury.is_changed() {
            dirty.mark_priority(member.faction_id);
        }
    }
    for cleared in injury_cleared.read() {
        if let Ok(member) = member_lookup.get(cleared) {
            dirty.mark_priority(member.faction_id);
        }
    }

    // Daily full-audit telemetry tick. Records that we did a sweep over
    // the population in the last day (always true with the every-tick
    // mark above) — leaves the counter wired for future producer-only
    // operation.
    let day = TICKS_PER_DAY as u64;
    if dirty.last_full_audit_tick == 0
        || now.saturating_sub(dirty.last_full_audit_tick) >= day
    {
        dirty.last_full_audit_tick = now;
        bg.opportunity_full_rebuilds = bg.opportunity_full_rebuilds.saturating_add(1);
    }

    // Drain a slice per tick.
    let cap = budget.opportunity_rebuilds_per_tick;
    let drained = dirty.drain_up_to(cap);
    let rebuilt = drained.len() as u32;

    // Pre-bucket injured by faction so each drained rebuild is O(injured_in_faction).
    let mut injured_by_faction: AHashMap<u32, Vec<(Entity, (i32, i32), u8)>> = AHashMap::default();
    if !drained.is_empty() {
        for (patient, member, transform, injury) in injured.iter() {
            if !drained.contains(&member.faction_id) {
                continue;
            }
            let tile = (
                (transform.translation.x / TILE_SIZE).floor() as i32,
                (transform.translation.y / TILE_SIZE).floor() as i32,
            );
            injured_by_faction
                .entry(member.faction_id)
                .or_default()
                .push((patient, tile, injury.severity));
        }
    }

    let expires_tick = now + OPPORTUNITY_TTL;

    for fid in &drained {
        let new_entries = rebuild_one_faction(
            *fid,
            &board,
            &registry,
            injured_by_faction.remove(fid).unwrap_or_default(),
            expires_tick,
        );
        index.replace_for_faction(*fid, new_entries);
    }
    index.rebuilt_at_tick = now;

    bg.opportunity_dirty_factions = dirty.factions.len() as u32;
    bg.opportunity_rebuilt_last_tick = rebuilt;
    bg.opportunity_entries = index.len() as u32;
    bg.opportunity_apply_us = crate::simulation::perf::micros_u32(t_start.elapsed());
}

/// Pure rebuild of one faction's bucket. Called once per faction per
/// per-tick drain step. Mirrors the legacy burst rebuild's 3 sub-passes.
fn rebuild_one_faction(
    fid: u32,
    board: &JobBoard,
    registry: &FactionRegistry,
    injured_in_faction: Vec<(Entity, (i32, i32), u8)>,
    expires_tick: u64,
) -> Vec<Opportunity> {
    let Some(faction) = registry.factions.get(&fid) else {
        return Vec::new();
    };
    let now = expires_tick.saturating_sub(OPPORTUNITY_TTL);
    let mut out: Vec<Opportunity> = Vec::new();

    // PaidJob — every unclaimed funded posting authored by this faction.
    if let Some(postings) = board.postings.get(&fid) {
        for posting in postings {
            if posting.reward <= 0.0 || !posting.claimants.is_empty() {
                continue;
            }
            if posting
                .expiry_tick
                .map(|expiry| now > expiry as u64)
                .unwrap_or(false)
            {
                continue;
            }
            out.push(Opportunity {
                kind: OpportunityKind::PaidJob,
                tile: faction.home_tile,
                faction_id: fid,
                payload: OpportunityPayload::PaidJob {
                    job_id: posting.id,
                    kind: posting.kind,
                    reward: posting.reward,
                    target: posting.progress.target_rid(),
                },
                expires_tick,
            });
        }
    }

    // FoodSource — any positive storage food.
    let food_qty = faction.storage.food_total() as u32;
    if food_qty > 0 {
        out.push(Opportunity {
            kind: OpportunityKind::FoodSource,
            tile: faction.home_tile,
            faction_id: fid,
            payload: OpportunityPayload::FoodSource { qty: food_qty },
            expires_tick,
        });
    }

    // MaterialDeficit — union of (material_targets, resource_demand) minus stock.
    let mut material_targets: AHashMap<ResourceId, u32> = AHashMap::default();
    for (&rid, &target) in faction.material_targets.iter() {
        material_targets.insert(rid, target);
    }
    for (&rid, &demand) in faction.resource_demand.iter() {
        let entry = material_targets.entry(rid).or_insert(0);
        *entry = (*entry).max(demand);
    }
    for (resource_id, target) in material_targets {
        let stored = faction.storage.stock_of(resource_id);
        let deficit = target.saturating_sub(stored);
        if deficit == 0 {
            continue;
        }
        out.push(Opportunity {
            kind: OpportunityKind::MaterialDeficit,
            tile: faction.home_tile,
            faction_id: fid,
            payload: OpportunityPayload::MaterialDeficit {
                resource_id,
                deficit,
            },
            expires_tick,
        });
    }

    // CareNeed — pre-bucketed by faction in the caller.
    for (patient, tile, severity) in injured_in_faction {
        out.push(Opportunity {
            kind: OpportunityKind::CareNeed,
            tile,
            faction_id: fid,
            payload: OpportunityPayload::CareNeed { patient, severity },
            expires_tick,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_buckets_by_kind_faction_and_region() {
        let mut index = OpportunityIndex::default();
        index.push(Opportunity {
            kind: OpportunityKind::PaidJob,
            tile: (33, 1),
            faction_id: 7,
            payload: OpportunityPayload::PaidJob {
                job_id: 1,
                kind: JobKind::Craft,
                reward: 10.0,
                target: None,
            },
            expires_tick: 10,
        });

        assert_eq!(index.iter_kind(OpportunityKind::PaidJob).count(), 1);
        assert_eq!(
            index
                .iter_kind_for_faction(7, OpportunityKind::PaidJob)
                .count(),
            1
        );
        assert_eq!(
            index
                .iter_kind_for_region((40, 2), OpportunityKind::PaidJob)
                .count(),
            1
        );
    }

    #[test]
    fn round_robin_drain_covers_every_faction_within_one_window() {
        // Equivalence guarantee: because each faction's bucket is rebuilt
        // independently and deterministically by `rebuild_one_faction`, the
        // incremental round-robin produces the same `per_faction` map as a
        // single full rebuild *iff* the drain covers every marked faction.
        // This asserts that coverage property over one `ceil(N/cap)` window.
        let mut dirty = OpportunityDirty::default();
        let factions = [3u32, 7, 11, 20, 42]; // non-contiguous ids
        let cap = 2;
        let window = (factions.len() + cap - 1) / cap; // ceil(5/2) = 3 ticks

        let mut covered: AHashSet<u32> = AHashSet::default();
        for _ in 0..window {
            // Each "tick" the rebuild system re-marks the whole population.
            dirty.mark_all(factions.iter().copied());
            for fid in dirty.drain_up_to(cap) {
                covered.insert(fid);
            }
        }
        for fid in factions {
            assert!(covered.contains(&fid), "faction {fid} never rebuilt");
        }
    }

    #[test]
    fn drain_never_exceeds_cap() {
        let mut dirty = OpportunityDirty::default();
        dirty.mark_all([1, 2, 3, 4, 5, 6, 7, 8]);
        dirty.mark_priority(5);
        dirty.mark_priority(6);
        assert_eq!(dirty.drain_up_to(3).len(), 3);
    }

    #[test]
    fn priority_lane_served_ahead_of_round_robin() {
        // A high faction id that the cursor would reach last still rebuilds
        // this tick when marked priority (injury → CareNeed latency).
        let mut dirty = OpportunityDirty::default();
        dirty.mark_all([1, 2, 3, 4, 99]);
        dirty.mark_priority(99);
        let drained = dirty.drain_up_to(2);
        assert!(drained.contains(&99), "priority faction must be in first drain");
        // 99 is consumed from both lanes — not re-drained next tick unless re-marked.
        assert!(!dirty.priority.contains(&99));
        assert!(!dirty.factions.contains(&99));
    }

    #[test]
    fn priority_pick_does_not_rewind_round_robin_cursor() {
        // Cursor parked at 50; a low-id priority pick must not rewind it,
        // otherwise the round-robin would re-serve already-covered ids.
        let mut dirty = OpportunityDirty::default();
        dirty.cursor = 50;
        dirty.mark_all([10, 60, 70, 80]);
        dirty.mark_priority(10); // low id, would rewind if it advanced cursor
        let _ = dirty.drain_up_to(2); // takes priority 10, then rr 60
        assert!(
            dirty.cursor > 50,
            "cursor advanced by round-robin pick (60), not rewound to the priority id"
        );
    }

    #[test]
    fn evict_expired_rebuilds_secondary_indexes() {
        let mut index = OpportunityIndex::default();
        index.push(Opportunity {
            kind: OpportunityKind::CareNeed,
            tile: (0, 0),
            faction_id: 1,
            payload: OpportunityPayload::CareNeed {
                patient: Entity::from_raw(1),
                severity: 20,
            },
            expires_tick: 5,
        });
        index.evict_expired(5);
        assert!(index.is_empty());
        assert_eq!(
            index
                .iter_kind_for_faction(1, OpportunityKind::CareNeed)
                .count(),
            0
        );
    }
}
