//! Faction / Settlement / Household-shared resource knowledge.
//!
//! Replaces the per-agent `AgentMemory.entries[32]` ring as the system of record
//! for *static* resource sightings (Wood, Stone, AnyEdible, Prey). Each tier
//! (`Household` < `Settlement` < `Faction`) owns an independent `KnowledgeMap`;
//! resources are stored as `ResourceCluster`s — influence-node aggregates that
//! collapse "all the trees in this forest" into one record. Vision feeds the
//! household tier directly; settlement and faction tiers receive promoted
//! clusters via `awareness_gossip_system` so what officials know depends on
//! whether bureaucrats / chiefs are physically present and socialising.
//!
//! Phase 1 ships only the data layer (resource + types + helpers + tests).
//! No production system reads or writes it yet — wiring lands in later phases.
//!
//! Reuses (do not duplicate):
//! - `ChunkCoord` / chunk math from `world::chunk` for the spatial index.
//! - The `MemoryKind` Copy enum from `simulation::memory`.
//! - `SettlementId` from `simulation::settlement`.

use crate::simulation::memory::MemoryKind;
use crate::simulation::settlement::SettlementId;
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
use ahash::{AHashMap, AHashSet};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

pub const CLUSTER_MERGE_RADIUS: i32 = 8;
pub const MAX_CLUSTER_RADIUS: u8 = 12;
pub const REPRESENTATIVE_TILES: usize = 4;
pub const CLUSTER_DECAY_TTL_TICKS: u64 = 3600 * 7; // 7 game-days
pub const CLUSTER_DECAY_CADENCE: u64 = 3600;       // 1 game-day

/// Stable identifier for a `ResourceCluster`. Allocated by `SharedKnowledge`
/// and never reused — depleted clusters are removed from every index but
/// outstanding `ClusterId` references silently degrade to "no longer known."
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct ClusterId(pub u32);

/// Who can harvest a remembered resource without committing theft. Wild
/// (default) means anyone; a stamped owner gates HTN dispatch through
/// `is_accessible_to`. Theft / raiding is a separate goal that explicitly
/// opens the filter — out of scope for this overhaul.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceOwner {
    Public,
    Person(Entity),
    Household(u32),
    Settlement(SettlementId),
    Faction(u32),
}

impl Default for ResourceOwner {
    fn default() -> Self {
        ResourceOwner::Public
    }
}

impl ResourceOwner {
    /// True when `viewer` (whose tier-set is described by the parameters) can
    /// harvest this resource without theft semantics.
    pub fn is_accessible_to(
        self,
        viewer: Entity,
        viewer_household: Option<u32>,
        viewer_settlement: Option<SettlementId>,
        viewer_faction: u32,
    ) -> bool {
        match self {
            ResourceOwner::Public => true,
            ResourceOwner::Person(p) => p == viewer,
            ResourceOwner::Household(h) => viewer_household == Some(h),
            ResourceOwner::Settlement(s) => viewer_settlement == Some(s),
            ResourceOwner::Faction(f) => viewer_faction == f,
        }
    }
}

/// Discriminated tier key for `SharedKnowledge.tiers`. Households see the
/// finest grain; settlement/faction maps materialise via gossip propagation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KnowledgeTier {
    Household(u32),
    Settlement(SettlementId),
    Faction(u32),
}

/// The set of tiers an agent's vision should write to and dispatch reads
/// should consult. Resolved per-agent at dispatch time from
/// `FactionMember`, `HouseholdMember`, and `SettlementMap::first_for_faction`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TierSet {
    pub household: Option<u32>,
    pub settlement: Option<SettlementId>,
    pub faction: u32,
}

impl TierSet {
    /// Iterate the tiers an agent participates in, finest first.
    pub fn tiers(&self) -> impl Iterator<Item = KnowledgeTier> {
        let h = self.household.map(KnowledgeTier::Household);
        let s = self.settlement.map(KnowledgeTier::Settlement);
        let f = Some(KnowledgeTier::Faction(self.faction));
        [h, s, f].into_iter().flatten()
    }
}

/// An aggregated influence node: a forest, a stone outcrop, a herd. Replaces
/// per-tile memory entries. Maintained by `report_sighting` / `report_depleted`
/// and decayed by `cluster_decay_system`.
#[derive(Clone, Debug)]
pub struct ResourceCluster {
    pub id: ClusterId,
    pub kind: MemoryKind,
    pub owner: ResourceOwner,
    /// Weighted centroid of member sightings.
    pub center: (i32, i32),
    /// Bounding chebyshev radius from `center`, capped at `MAX_CLUSTER_RADIUS`.
    pub radius: u8,
    /// Most recent member sightings, fixed-size LRU. `None` slots are empty.
    pub representative_tiles: [Option<(i32, i32)>; REPRESENTATIVE_TILES],
    pub estimated_count: u16,
    pub last_seen_tick: u64,
}

impl ResourceCluster {
    fn new(id: ClusterId, kind: MemoryKind, owner: ResourceOwner, tile: (i32, i32), now: u64) -> Self {
        let mut rep = [None; REPRESENTATIVE_TILES];
        rep[0] = Some(tile);
        Self {
            id,
            kind,
            owner,
            center: tile,
            radius: 1,
            representative_tiles: rep,
            estimated_count: 1,
            last_seen_tick: now,
        }
    }

    /// Push a tile into the LRU rep buffer. If already present, move-to-head.
    fn push_rep(&mut self, tile: (i32, i32)) {
        if self.representative_tiles.iter().any(|s| *s == Some(tile)) {
            // Already known as a rep; bump it to slot 0.
            for slot in self.representative_tiles.iter_mut() {
                if *slot == Some(tile) {
                    *slot = None;
                    break;
                }
            }
        }
        // Shift right, drop the tail.
        for i in (1..REPRESENTATIVE_TILES).rev() {
            self.representative_tiles[i] = self.representative_tiles[i - 1];
        }
        self.representative_tiles[0] = Some(tile);
    }

    fn drop_rep(&mut self, tile: (i32, i32)) {
        for slot in self.representative_tiles.iter_mut() {
            if *slot == Some(tile) {
                *slot = None;
            }
        }
    }

    /// Closest representative tile to `from` by chebyshev. Falls back to
    /// `center` if all rep slots are empty.
    pub fn nearest_target_tile(&self, from: (i32, i32)) -> (i32, i32) {
        let mut best: Option<((i32, i32), i32)> = None;
        for slot in self.representative_tiles.iter() {
            if let Some(t) = *slot {
                let d = (t.0 - from.0).abs().max((t.1 - from.1).abs());
                if best.map_or(true, |(_, bd)| d < bd) {
                    best = Some((t, d));
                }
            }
        }
        best.map(|(t, _)| t).unwrap_or(self.center)
    }

    fn chunk_of(tile: (i32, i32)) -> ChunkCoord {
        ChunkCoord(
            tile.0.div_euclid(CHUNK_SIZE as i32),
            tile.1.div_euclid(CHUNK_SIZE as i32),
        )
    }

    /// Set of chunks touched by this cluster (for the chunk index).
    fn chunk_footprint(&self) -> Vec<ChunkCoord> {
        let r = self.radius as i32;
        let (cx, cy) = self.center;
        let lo = Self::chunk_of((cx - r, cy - r));
        let hi = Self::chunk_of((cx + r, cy + r));
        let mut out = Vec::with_capacity(((hi.0 - lo.0 + 1) * (hi.1 - lo.1 + 1)) as usize);
        for x in lo.0..=hi.0 {
            for y in lo.1..=hi.1 {
                out.push(ChunkCoord(x, y));
            }
        }
        out
    }
}

/// Per-tier knowledge map. One per `KnowledgeTier`. Holds the cluster
/// records plus two indices for fast lookup: by_kind for "every cluster of
/// this resource" and by_chunk for spiral-search nearest queries.
#[derive(Default, Debug)]
pub struct KnowledgeMap {
    pub clusters: AHashMap<ClusterId, ResourceCluster>,
    pub by_kind: AHashMap<MemoryKind, AHashSet<ClusterId>>,
    pub by_chunk: AHashMap<ChunkCoord, AHashSet<ClusterId>>,
}

impl KnowledgeMap {
    fn add_to_indices(&mut self, c: &ResourceCluster) {
        self.by_kind.entry(c.kind).or_default().insert(c.id);
        for ch in c.chunk_footprint() {
            self.by_chunk.entry(ch).or_default().insert(c.id);
        }
    }

    fn remove_from_indices(&mut self, c: &ResourceCluster) {
        if let Some(s) = self.by_kind.get_mut(&c.kind) {
            s.remove(&c.id);
            if s.is_empty() {
                self.by_kind.remove(&c.kind);
            }
        }
        for ch in c.chunk_footprint() {
            if let Some(s) = self.by_chunk.get_mut(&ch) {
                s.remove(&c.id);
                if s.is_empty() {
                    self.by_chunk.remove(&ch);
                }
            }
        }
    }

    /// Find an existing cluster of `(kind, owner)` whose center is within
    /// `CLUSTER_MERGE_RADIUS` of `tile`. O(candidates near `tile`'s chunk).
    fn find_mergeable(&self, tile: (i32, i32), kind: MemoryKind, owner: ResourceOwner) -> Option<ClusterId> {
        let chunk = ResourceCluster::chunk_of(tile);
        // Scan the 3×3 chunk neighbourhood (CLUSTER_MERGE_RADIUS=8 ≤ CHUNK_SIZE=32,
        // so candidate centers always fall within ±1 chunk).
        let mut best: Option<(ClusterId, i32)> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let ch = ChunkCoord(chunk.0 + dx, chunk.1 + dy);
                let Some(set) = self.by_chunk.get(&ch) else { continue };
                for &cid in set {
                    let Some(c) = self.clusters.get(&cid) else { continue };
                    if c.kind != kind || c.owner != owner {
                        continue;
                    }
                    let d = (c.center.0 - tile.0).abs().max((c.center.1 - tile.1).abs());
                    if d <= CLUSTER_MERGE_RADIUS && best.map_or(true, |(_, bd)| d < bd) {
                        best = Some((cid, d));
                    }
                }
            }
        }
        best.map(|(c, _)| c)
    }

    /// Look up the cluster (if any) that contains `tile` for `kind`. Used by
    /// depletion reports — finds the closest cluster center within its radius.
    fn cluster_at(&self, tile: (i32, i32), kind: MemoryKind) -> Option<ClusterId> {
        let chunk = ResourceCluster::chunk_of(tile);
        let mut best: Option<(ClusterId, i32)> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let ch = ChunkCoord(chunk.0 + dx, chunk.1 + dy);
                let Some(set) = self.by_chunk.get(&ch) else { continue };
                for &cid in set {
                    let Some(c) = self.clusters.get(&cid) else { continue };
                    if c.kind != kind {
                        continue;
                    }
                    let d = (c.center.0 - tile.0).abs().max((c.center.1 - tile.1).abs());
                    if d <= c.radius as i32 && best.map_or(true, |(_, bd)| d < bd) {
                        best = Some((cid, d));
                    }
                }
            }
        }
        best.map(|(c, _)| c)
    }

    /// Spiral search outward from `from`'s chunk for clusters of `kind`
    /// whose `owner_filter` accepts. Returns the closest by chebyshev
    /// distance to the cluster bounding edge, with claim_score subtracted.
    /// `max_chunk_radius` caps how far we search.
    pub fn nearest<F: Fn(ResourceOwner) -> bool, P: Fn((i32, i32)) -> i32>(
        &self,
        kind: MemoryKind,
        from: (i32, i32),
        owner_filter: F,
        claim_penalty: P,
        max_chunk_radius: i32,
    ) -> Option<ClusterId> {
        let origin = ResourceCluster::chunk_of(from);
        let mut best: Option<(ClusterId, i32)> = None;
        for r in 0..=max_chunk_radius {
            // Walk the ring at chunk-distance `r`.
            let lo = -r;
            let hi = r;
            for dx in lo..=hi {
                for dy in lo..=hi {
                    if dx.abs().max(dy.abs()) != r {
                        continue;
                    }
                    let ch = ChunkCoord(origin.0 + dx, origin.1 + dy);
                    let Some(set) = self.by_chunk.get(&ch) else { continue };
                    for &cid in set {
                        let Some(c) = self.clusters.get(&cid) else { continue };
                        if c.kind != kind || !owner_filter(c.owner) {
                            continue;
                        }
                        let target = c.nearest_target_tile(from);
                        let raw = (target.0 - from.0).abs().max((target.1 - from.1).abs());
                        let score = raw + claim_penalty(target);
                        if best.map_or(true, |(_, bs)| score < bs) {
                            best = Some((cid, score));
                        }
                    }
                }
            }
            // Early-out: if we found a cluster on a ring closer than the next
            // ring's inner edge (= r+1 chunks ≈ (r+1)*CHUNK_SIZE tiles),
            // anything farther can't beat it (claim penalties aside).
            if let Some((_, bs)) = best {
                if bs < (r + 1) * CHUNK_SIZE as i32 {
                    break;
                }
            }
        }
        best.map(|(c, _)| c)
    }
}

/// Marker stamped on `Plant` entities (and any future tile-bound resource)
/// recording who owns them. Absence of `LandClaim` ⇒ `ResourceOwner::Public`
/// (wilderness). Inserted at planting in `production_system`'s Planter arm
/// based on the planter's tier set; flows into the cluster's `ResourceOwner`
/// when `vision_system` writes the sighting.
#[derive(Component, Clone, Copy, Debug)]
pub struct LandClaim {
    pub owner: ResourceOwner,
}

/// Resource-level holder of per-tier `KnowledgeMap`s. Single source of truth
/// for "what resources exist where, and who knows about them." Replaces the
/// per-agent `AgentMemory.entries[32]` ring for static resource queries.
#[derive(Resource, Default)]
pub struct SharedKnowledge {
    pub tiers: AHashMap<KnowledgeTier, KnowledgeMap>,
    next_id: u32,
}

impl SharedKnowledge {
    fn alloc_id(&mut self) -> ClusterId {
        let id = ClusterId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    pub fn map(&self, tier: KnowledgeTier) -> Option<&KnowledgeMap> {
        self.tiers.get(&tier)
    }

    pub fn map_mut(&mut self, tier: KnowledgeTier) -> &mut KnowledgeMap {
        self.tiers.entry(tier).or_default()
    }

    /// Record that `tile` holds `kind` owned by `owner`, observed at `now`.
    /// Merges into an existing cluster of the same `(kind, owner)` within
    /// `CLUSTER_MERGE_RADIUS`; otherwise creates a new singleton cluster.
    /// Returns the cluster id touched.
    pub fn report_sighting(
        &mut self,
        tier: KnowledgeTier,
        tile: (i32, i32),
        kind: MemoryKind,
        owner: ResourceOwner,
        now: u64,
    ) -> ClusterId {
        // Check for mergeable using only the map, then mutate.
        let existing = {
            let m = self.tiers.entry(tier).or_default();
            m.find_mergeable(tile, kind, owner)
        };
        if let Some(cid) = existing {
            // Borrow the cluster, decide how its footprint changes, then re-index.
            let m = self.tiers.get_mut(&tier).expect("tier entry just inserted");
            let old_footprint = m.clusters.get(&cid).map(|c| c.chunk_footprint()).unwrap_or_default();
            if let Some(c) = m.clusters.get_mut(&cid) {
                let already = c.representative_tiles.iter().any(|s| *s == Some(tile));
                c.push_rep(tile);
                c.last_seen_tick = now;
                if !already {
                    c.estimated_count = c.estimated_count.saturating_add(1);
                    // Grow radius to fit the new tile.
                    let dx = (c.center.0 - tile.0).abs();
                    let dy = (c.center.1 - tile.1).abs();
                    let needed = dx.max(dy) as u8;
                    if needed > c.radius {
                        c.radius = needed.min(MAX_CLUSTER_RADIUS);
                    }
                }
            }
            // Re-index if footprint changed.
            let new_footprint = m.clusters.get(&cid).map(|c| c.chunk_footprint()).unwrap_or_default();
            if old_footprint != new_footprint {
                for ch in &old_footprint {
                    if let Some(s) = m.by_chunk.get_mut(ch) {
                        s.remove(&cid);
                        if s.is_empty() {
                            m.by_chunk.remove(ch);
                        }
                    }
                }
                for ch in &new_footprint {
                    m.by_chunk.entry(*ch).or_default().insert(cid);
                }
            }
            cid
        } else {
            let id = self.alloc_id();
            let cluster = ResourceCluster::new(id, kind, owner, tile, now);
            let m = self.tiers.entry(tier).or_default();
            m.add_to_indices(&cluster);
            m.clusters.insert(id, cluster);
            id
        }
    }

    /// Record that `tile` no longer holds `kind` (harvested, depleted, etc.).
    /// Decrements the containing cluster's `estimated_count`; when the count
    /// hits zero, despawns the cluster from every index.
    pub fn report_depleted(&mut self, tier: KnowledgeTier, tile: (i32, i32), kind: MemoryKind) {
        let Some(m) = self.tiers.get_mut(&tier) else { return };
        let Some(cid) = m.cluster_at(tile, kind) else { return };
        let despawn;
        let removed_cluster: Option<ResourceCluster>;
        {
            let Some(c) = m.clusters.get_mut(&cid) else { return };
            c.drop_rep(tile);
            c.estimated_count = c.estimated_count.saturating_sub(1);
            despawn = c.estimated_count == 0;
        }
        if despawn {
            removed_cluster = m.clusters.remove(&cid);
            if let Some(c) = removed_cluster {
                m.remove_from_indices(&c);
            }
        }
    }

    /// Drop clusters not refreshed in `CLUSTER_DECAY_TTL_TICKS`. Called by
    /// `cluster_decay_system` once per day.
    pub fn decay(&mut self, now: u64) {
        for m in self.tiers.values_mut() {
            let stale: Vec<ClusterId> = m
                .clusters
                .iter()
                .filter(|(_, c)| now.saturating_sub(c.last_seen_tick) > CLUSTER_DECAY_TTL_TICKS)
                .map(|(id, _)| *id)
                .collect();
            for id in stale {
                if let Some(c) = m.clusters.remove(&id) {
                    m.remove_from_indices(&c);
                }
            }
        }
    }

    /// Convenience: walk an agent's `TierSet` finest-first and return the
    /// first nearest accessible cluster found. `claim_penalty` lets callers
    /// downweight tiles already claimed by other agents.
    pub fn nearest_in_tier_set<F: Fn(ResourceOwner) -> bool, P: Fn((i32, i32)) -> i32>(
        &self,
        tier_set: TierSet,
        kind: MemoryKind,
        from: (i32, i32),
        owner_filter: F,
        claim_penalty: P,
        max_chunk_radius: i32,
    ) -> Option<(KnowledgeTier, ClusterId, (i32, i32))> {
        for tier in tier_set.tiers() {
            let Some(m) = self.tiers.get(&tier) else { continue };
            if let Some(cid) = m.nearest(kind, from, &owner_filter, &claim_penalty, max_chunk_radius) {
                let target = m.clusters.get(&cid).map(|c| c.nearest_target_tile(from)).unwrap_or(from);
                return Some((tier, cid, target));
            }
        }
        None
    }
}

/// Bundled `SystemParam` for the three resources HTN dispatchers need to do
/// shared-knowledge lookups. Collapses three params into one so dispatchers
/// don't trip Bevy's 16-param ceiling on already-large `Query` lists.
#[derive(SystemParam)]
pub struct GatherKnowledge<'w> {
    pub shared: Res<'w, SharedKnowledge>,
    pub settlement_map: Res<'w, crate::simulation::settlement::SettlementMap>,
    pub claims: Res<'w, crate::simulation::gather_claims::GatherClaims>,
}

impl<'w> GatherKnowledge<'w> {
    /// Convenience: nearest accessible cluster's target tile for an agent
    /// described by `(actor, faction_id, household_id)`. Wraps tier-set
    /// resolution + owner filter + claim-pressure penalty.
    pub fn nearest_target_tile(
        &self,
        actor: Entity,
        faction_id: u32,
        household_id: Option<u32>,
        kind: MemoryKind,
        from: (i32, i32),
        now: u64,
    ) -> Option<(i32, i32)> {
        let tier_set = agent_tier_set(faction_id, household_id, &self.settlement_map);
        let viewer_settlement = self.settlement_map.first_for_faction(faction_id);
        let owner_filter = move |o: ResourceOwner| {
            o.is_accessible_to(actor, household_id, viewer_settlement, faction_id)
        };
        let claim_pen = |t: (i32, i32)| self.claims.pressure(t, now, actor) * 4;
        self.shared
            .nearest_in_tier_set(tier_set, kind, from, owner_filter, claim_pen, 16)
            .map(|(_, _, tile)| tile)
    }
}

/// Resolve the agent's tier-set: household if the agent is a HouseholdMember,
/// settlement if the faction has a registered settlement, faction always.
/// SOLO agents (faction_id == 0 / `SOLO`) get a Faction(0) tier only — no
/// household, no settlement — so writes and reads still funnel through one
/// stable bucket.
pub fn agent_tier_set(
    faction_id: u32,
    household_id: Option<u32>,
    settlement_map: &crate::simulation::settlement::SettlementMap,
) -> TierSet {
    let settlement = settlement_map.first_for_faction(faction_id);
    TierSet {
        household: household_id,
        settlement,
        faction: faction_id,
    }
}

/// True if any cluster of `kind` is known to the faction (Faction tier or
/// any Settlement owned by the faction) within `max_chunk_radius` chunks of
/// `from`. Used by `chief_job_posting_system` (Phase 8) to gate Stockpile
/// postings on real local availability — no faction-tier sighting near home
/// ⇒ chief skips the posting and the resource flows through markets / traders
/// instead of communal labour.
///
/// Walks the faction's settlements via `SettlementMap::for_faction`. SOLO /
/// settlement-less factions consult only the `Faction(fid)` tier.
pub fn faction_knows_cluster(
    shared: &SharedKnowledge,
    settlement_map: &crate::simulation::settlement::SettlementMap,
    faction_id: u32,
    kind: MemoryKind,
    from: (i32, i32),
    max_chunk_radius: i32,
) -> bool {
    let owned_settlements = settlement_map.for_faction(faction_id).to_vec();
    let owner_filter = |o: ResourceOwner| match o {
        ResourceOwner::Public => true,
        ResourceOwner::Faction(fid) => fid == faction_id,
        ResourceOwner::Settlement(sid) => owned_settlements.contains(&sid),
        // Households / individuals are private — chief can't direct labor
        // at them. (Theft path opens this filter; out of scope here.)
        ResourceOwner::Person(_) | ResourceOwner::Household(_) => false,
    };
    let no_pen = |_t: (i32, i32)| 0;

    if let Some(m) = shared.tiers.get(&KnowledgeTier::Faction(faction_id)) {
        if m.nearest(kind, from, &owner_filter, &no_pen, max_chunk_radius).is_some() {
            return true;
        }
    }
    for &sid in &owned_settlements {
        if let Some(m) = shared.tiers.get(&KnowledgeTier::Settlement(sid)) {
            if m.nearest(kind, from, &owner_filter, &no_pen, max_chunk_radius).is_some() {
                return true;
            }
        }
    }
    false
}

/// Once-per-day decay tick. Registered in Economy schedule by callers.
pub fn cluster_decay_system(
    clock: Res<crate::simulation::SimClock>,
    mut shared: ResMut<SharedKnowledge>,
) {
    if clock.tick % CLUSTER_DECAY_CADENCE != 0 {
        return;
    }
    shared.decay(clock.tick);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faction_tier(id: u32) -> KnowledgeTier {
        KnowledgeTier::Faction(id)
    }

    fn fake_kind() -> MemoryKind {
        // AnyEdible is the one MemoryKind variant that doesn't depend on
        // core_ids being initialised — usable in unit tests without spinning
        // up the catalog. Wood/Stone exercise the same code path; they're
        // covered by integration tests in test_fixture.
        MemoryKind::AnyEdible
    }

    #[test]
    fn singleton_sighting_creates_cluster() {
        let mut sk = SharedKnowledge::default();
        let cid = sk.report_sighting(faction_tier(1), (10, 10), fake_kind(), ResourceOwner::Public, 0);
        let m = sk.map(faction_tier(1)).unwrap();
        assert_eq!(m.clusters.len(), 1);
        let c = &m.clusters[&cid];
        assert_eq!(c.center, (10, 10));
        assert_eq!(c.radius, 1);
        assert_eq!(c.estimated_count, 1);
        assert_eq!(c.representative_tiles[0], Some((10, 10)));
    }

    #[test]
    fn nearby_sightings_merge_into_one_cluster() {
        let mut sk = SharedKnowledge::default();
        let a = sk.report_sighting(faction_tier(1), (10, 10), fake_kind(), ResourceOwner::Public, 0);
        let b = sk.report_sighting(faction_tier(1), (12, 11), fake_kind(), ResourceOwner::Public, 0);
        assert_eq!(a, b, "tiles within CLUSTER_MERGE_RADIUS should merge");
        let m = sk.map(faction_tier(1)).unwrap();
        assert_eq!(m.clusters.len(), 1);
        let c = &m.clusters[&a];
        assert_eq!(c.estimated_count, 2);
        assert!(c.representative_tiles.iter().any(|s| *s == Some((12, 11))));
    }

    #[test]
    fn distant_sightings_create_separate_clusters() {
        let mut sk = SharedKnowledge::default();
        let a = sk.report_sighting(faction_tier(1), (0, 0), fake_kind(), ResourceOwner::Public, 0);
        let b = sk.report_sighting(faction_tier(1), (50, 50), fake_kind(), ResourceOwner::Public, 0);
        assert_ne!(a, b);
        assert_eq!(sk.map(faction_tier(1)).unwrap().clusters.len(), 2);
    }

    #[test]
    fn different_owners_do_not_merge() {
        let mut sk = SharedKnowledge::default();
        let a = sk.report_sighting(faction_tier(1), (10, 10), fake_kind(), ResourceOwner::Public, 0);
        let b = sk.report_sighting(faction_tier(1), (11, 10), fake_kind(), ResourceOwner::Faction(7), 0);
        assert_ne!(a, b, "different owners must be tracked separately");
    }

    #[test]
    fn depletion_decrements_then_despawns_cluster() {
        let mut sk = SharedKnowledge::default();
        sk.report_sighting(faction_tier(1), (10, 10), fake_kind(), ResourceOwner::Public, 0);
        sk.report_sighting(faction_tier(1), (12, 11), fake_kind(), ResourceOwner::Public, 0);
        let m = sk.map(faction_tier(1)).unwrap();
        let cid = *m.clusters.keys().next().unwrap();
        assert_eq!(m.clusters[&cid].estimated_count, 2);

        sk.report_depleted(faction_tier(1), (12, 11), fake_kind());
        let m = sk.map(faction_tier(1)).unwrap();
        assert_eq!(m.clusters[&cid].estimated_count, 1);

        sk.report_depleted(faction_tier(1), (10, 10), fake_kind());
        let m = sk.map(faction_tier(1)).unwrap();
        assert!(m.clusters.is_empty(), "cluster should despawn at count 0");
        assert!(m.by_kind.is_empty());
        assert!(m.by_chunk.is_empty());
    }

    #[test]
    fn nearest_picks_closest_cluster() {
        let mut sk = SharedKnowledge::default();
        let near = sk.report_sighting(faction_tier(1), (5, 5), fake_kind(), ResourceOwner::Public, 0);
        let _far = sk.report_sighting(faction_tier(1), (200, 200), fake_kind(), ResourceOwner::Public, 0);
        let m = sk.map(faction_tier(1)).unwrap();
        let got = m.nearest(fake_kind(), (0, 0), |_| true, |_| 0, 32).unwrap();
        assert_eq!(got, near);
    }

    #[test]
    fn nearest_respects_owner_filter() {
        let mut sk = SharedKnowledge::default();
        let _public_far = sk.report_sighting(faction_tier(1), (50, 50), fake_kind(), ResourceOwner::Public, 0);
        let private_near = sk.report_sighting(faction_tier(1), (5, 5), fake_kind(), ResourceOwner::Faction(7), 0);
        let m = sk.map(faction_tier(1)).unwrap();
        let got = m.nearest(fake_kind(), (0, 0), |o| matches!(o, ResourceOwner::Public), |_| 0, 32).unwrap();
        let cluster = &m.clusters[&got];
        assert!(matches!(cluster.owner, ResourceOwner::Public));
        assert_ne!(got, private_near);
    }

    #[test]
    fn nearest_in_tier_set_walks_finest_first() {
        let mut sk = SharedKnowledge::default();
        // Faction-tier knows a far one; household-tier knows a near one.
        sk.report_sighting(KnowledgeTier::Faction(1), (100, 100), fake_kind(), ResourceOwner::Public, 0);
        sk.report_sighting(KnowledgeTier::Household(42), (3, 3), fake_kind(), ResourceOwner::Public, 0);
        let ts = TierSet { household: Some(42), settlement: None, faction: 1 };
        let (tier, _cid, target) = sk.nearest_in_tier_set(ts, fake_kind(), (0, 0), |_| true, |_| 0, 32).unwrap();
        assert_eq!(tier, KnowledgeTier::Household(42));
        assert_eq!(target, (3, 3));
    }

    #[test]
    fn decay_removes_stale_clusters() {
        let mut sk = SharedKnowledge::default();
        sk.report_sighting(faction_tier(1), (10, 10), fake_kind(), ResourceOwner::Public, 0);
        sk.report_sighting(faction_tier(1), (1000, 1000), fake_kind(), ResourceOwner::Public, CLUSTER_DECAY_TTL_TICKS + 100);
        sk.decay(CLUSTER_DECAY_TTL_TICKS + 100);
        let m = sk.map(faction_tier(1)).unwrap();
        assert_eq!(m.clusters.len(), 1);
        assert!(m.clusters.values().all(|c| c.center == (1000, 1000)));
    }

    #[test]
    fn sighting_grows_cluster_radius() {
        let mut sk = SharedKnowledge::default();
        let a = sk.report_sighting(faction_tier(1), (10, 10), fake_kind(), ResourceOwner::Public, 0);
        sk.report_sighting(faction_tier(1), (15, 14), fake_kind(), ResourceOwner::Public, 0);
        let c = &sk.map(faction_tier(1)).unwrap().clusters[&a];
        assert!(c.radius >= 5);
    }

    #[test]
    fn radius_capped_at_max() {
        // Build a chain of sightings extending far beyond MAX_CLUSTER_RADIUS;
        // each one should merge with the previous (within MERGE_RADIUS), but
        // radius is capped.
        let mut sk = SharedKnowledge::default();
        sk.report_sighting(faction_tier(1), (0, 0), fake_kind(), ResourceOwner::Public, 0);
        // Each step must be ≤ CLUSTER_MERGE_RADIUS from the *current center*,
        // and the center is the original sighting (we don't recompute it),
        // so a few merges still cap the radius at MAX_CLUSTER_RADIUS.
        sk.report_sighting(faction_tier(1), (8, 0), fake_kind(), ResourceOwner::Public, 0);
        let c = sk.map(faction_tier(1)).unwrap().clusters.values().next().unwrap();
        assert!(c.radius <= MAX_CLUSTER_RADIUS);
    }

    #[test]
    fn is_accessible_to_rules() {
        let dummy = Entity::from_raw(42);
        let other = Entity::from_raw(99);
        assert!(ResourceOwner::Public.is_accessible_to(dummy, None, None, 0));
        assert!(ResourceOwner::Person(dummy).is_accessible_to(dummy, None, None, 0));
        assert!(!ResourceOwner::Person(other).is_accessible_to(dummy, None, None, 0));
        assert!(ResourceOwner::Household(7).is_accessible_to(dummy, Some(7), None, 0));
        assert!(!ResourceOwner::Household(7).is_accessible_to(dummy, Some(8), None, 0));
        assert!(!ResourceOwner::Household(7).is_accessible_to(dummy, None, None, 0));
        assert!(ResourceOwner::Faction(3).is_accessible_to(dummy, None, None, 3));
        assert!(!ResourceOwner::Faction(3).is_accessible_to(dummy, None, None, 4));
    }
}
