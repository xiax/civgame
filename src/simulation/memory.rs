use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::person::Person;
use super::schedule::{BucketSlot, SimClock};
use super::shared_knowledge::ResourceOwner;
use super::social_contact::{is_social_contact, SecondarySocial};
use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use bevy::prelude::*;

/// What an `AgentMemory` entry refers to. Phase 2-residual #3 collapsed the
/// per-good `Food` / `Wood` / `Stone` / `GrainSeed` / `BerrySeed` variants
/// into a single `Resource(ResourceId)` so any catalog resource can be
/// remembered without an enum change. `AnyEdible` survives as the
/// "see any food" aggregate read by AcquireFood / StockpileFood / Forage —
/// vision writes one `AnyEdible` entry per visible food so the dispatcher
/// can pick the closest without iterating every edible `ResourceId`. Adding
/// a second class-level aggregate (e.g. "any building material") would land
/// as a new variant; today AnyEdible is the only one with a consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryKind {
    AnyEdible,
    Resource(ResourceId),
    Prey,
    /// A sighted wild herd (Deer / Horse / Cattle). Distinct from `Prey`
    /// (which mixes predators and game): nomad migration scoring reads
    /// `HerdSighting` clusters as a *knowledge-gated* "good grazing /
    /// hunting nearby" signal — a band only migrates toward herds it has
    /// actually scouted, not every herd on the map.
    HerdSighting,
    /// A sighted hostile faction war party. `nomad::score_danger` reads
    /// these clusters as a real migration deterrent (replaces the former
    /// misuse of `Prey` as a danger proxy).
    HostileFactionSighting,
}

impl MemoryKind {
    /// `MemoryKind::Resource(WOOD)` constructed via the global core-id cache.
    /// Panics if `init_core_ids` hasn't run (every production / test path
    /// installs the catalog at startup).
    pub fn wood() -> Self {
        Self::Resource(
            *core_ids::Wood
                .get()
                .expect("MemoryKind::wood: core_ids not initialised"),
        )
    }
    pub fn stone() -> Self {
        Self::Resource(
            *core_ids::Stone
                .get()
                .expect("MemoryKind::stone: core_ids not initialised"),
        )
    }
    pub fn grain_seed() -> Self {
        Self::Resource(
            *core_ids::GrainSeed
                .get()
                .expect("MemoryKind::grain_seed: core_ids not initialised"),
        )
    }
    pub fn berry_seed() -> Self {
        Self::Resource(
            *core_ids::BerrySeed
                .get()
                .expect("MemoryKind::berry_seed: core_ids not initialised"),
        )
    }
    /// `MemoryKind::Resource(REEDS)` — wetland reed-bed sightings. Phase
    /// F.2 vision reports any `TileKind::Marsh` as a reeds source so
    /// the AcquireGood/Stockpile chain can route a worker to harvest
    /// `reeds` for construction recipes (Wattle-and-Daub trim, Reed
    /// Matting, Thatch Roofing's reed-rope binding, etc.).
    pub fn reeds() -> Self {
        Self::Resource(
            *core_ids::Reeds
                .get()
                .expect("MemoryKind::reeds: core_ids not initialised"),
        )
    }

    /// True for every kind whose semantic meaning is "this tile holds food."
    pub fn is_any_edible(self) -> bool {
        matches!(self, MemoryKind::AnyEdible)
    }
    pub fn is_wood(self) -> bool {
        self == MemoryKind::wood()
    }
    pub fn is_stone(self) -> bool {
        self == MemoryKind::stone()
    }
}

/// Per-agent personal memory. Resource sightings live in `SharedKnowledge`
/// (Phase 7 of the memory overhaul retired the 32-slot tile ring); this
/// struct now keeps only the personal/social context the shared map can't
/// carry — visited settlements (R8) and a soft hint of the cluster the
/// agent's current dispatch chain is anchored to.
#[derive(Component, Clone, Default)]
pub struct AgentMemory {
    /// Pluralist Economy R8 — region-aware memory. Tracks
    /// `(SettlementId, freshness)` for up to 8 settlements the
    /// agent has visited or heard about. R10's Trader dispatcher
    /// walks pairs in this slot for arbitrage.
    pub visited_settlements: [Option<(crate::simulation::settlement::SettlementId, u8)>; 8],
}

impl AgentMemory {
    /// Pluralist Economy R8: record a visited / heard-about settlement.
    /// Idempotent — re-recording the same id resets the freshness to 255.
    /// When the slot ring is full, evicts the lowest-freshness entry.
    pub fn record_settlement(&mut self, id: crate::simulation::settlement::SettlementId) {
        for slot in &mut self.visited_settlements {
            if let Some((existing, fresh)) = slot {
                if *existing == id {
                    *fresh = 255;
                    return;
                }
            }
        }
        for slot in &mut self.visited_settlements {
            if slot.is_none() {
                *slot = Some((id, 255));
                return;
            }
        }
        let mut min_fresh = u8::MAX;
        let mut min_idx = 0usize;
        for (i, slot) in self.visited_settlements.iter().enumerate() {
            if let Some((_, f)) = slot {
                if *f < min_fresh {
                    min_fresh = *f;
                    min_idx = i;
                }
            }
        }
        self.visited_settlements[min_idx] = Some((id, 255));
    }

    /// Pluralist Economy R8: read every remembered settlement,
    /// freshest first. Used by R10's Trader dispatcher to walk
    /// pairs of remembered markets for arbitrage decisions.
    pub fn known_settlements(
        &self,
    ) -> impl Iterator<Item = (crate::simulation::settlement::SettlementId, u8)> + '_ {
        self.visited_settlements.iter().filter_map(|s| *s)
    }
}

/// One entry in `CurrentVision`. Records what an agent saw during its most
/// recent `vision_system` pass: kind + tile + (optional) entity + owner. The
/// `entity` slot is `Some` for entity-anchored sightings (`GroundItem`, prey)
/// where dispatchers need the entity handle, and `None` for tile-anchored
/// sightings (mature plant, stone tile) where only the tile matters.
#[derive(Clone, Copy, Debug)]
pub struct VisionEntry {
    pub kind: MemoryKind,
    pub tile: (i32, i32),
    pub entity: Option<Entity>,
    pub owner: ResourceOwner,
}

/// Per-agent "what I see right now" buffer. Refilled each time the agent's
/// `BucketSlot` fires (≤20 ticks ≈ 1s old at dispatch time). Dispatchers
/// consult this *before* `SharedKnowledge` so a worker standing within sight
/// of a viable target picks it deterministically rather than walking to a
/// stale remembered cluster.
///
/// Vision still writes through to `SharedKnowledge` (additively) so other
/// agents can benefit via gossip / tier promotion. This buffer is a separate,
/// agent-private channel — it never feeds depletion semantics, preserving
/// the "vision is additive" invariant.
#[derive(Component, Clone, Default)]
pub struct CurrentVision {
    pub entries: Vec<VisionEntry>,
}

impl CurrentVision {
    pub fn iter_kind(&self, kind: MemoryKind) -> impl Iterator<Item = &VisionEntry> + '_ {
        self.entries.iter().filter(move |v| v.kind == kind)
    }

    /// Pick the nearest visible tile-anchored target (`entity == None`) of the
    /// requested kind that the viewer can harvest without theft. Used as the
    /// vision-first short-circuit for gather methods before falling back to
    /// `SharedKnowledge`.
    ///
    /// `dist` returns the detour-aware distance (in chebyshev-equivalent
    /// tiles) from the agent to a candidate tile — `DetourEstimator::from`
    /// at the call site, so a river forcing a long walk-around is priced
    /// in instead of straight-line distance.
    ///
    /// `claim_penalty` returns extra cost (in chebyshev tiles) to add for a
    /// candidate tile — typically `GatherClaims::pressure(tile, now, viewer) * 4`
    /// at the call site, mirroring `SharedKnowledge::nearest_target_tile`'s
    /// claim weight so two paths score consistently. Pass `|_| 0` to opt out.
    pub fn nearest_gather_target(
        &self,
        kind: MemoryKind,
        dist: impl Fn((i32, i32)) -> i32,
        viewer: Entity,
        viewer_household: Option<u32>,
        viewer_settlement: Option<crate::simulation::settlement::SettlementId>,
        viewer_faction: u32,
        claim_penalty: impl Fn((i32, i32)) -> i32,
        is_reachable: impl Fn((i32, i32)) -> bool,
    ) -> Option<(i32, i32)> {
        // Phase 2a: prefer reachable targets; if every visible candidate is
        // in a disconnected chunk, fall back to the unfiltered nearest so
        // the dispatcher doesn't suddenly emit nothing for an agent who
        // stepped into a momentarily-unbuilt-graph chunk. Matches the
        // fallback behaviour of `StorageTileMap::nearest_for_faction_reachable`.
        let pick = |require_reachable: bool| -> Option<(i32, i32)> {
            self.iter_kind(kind)
                .filter(|v| v.entity.is_none())
                .filter(|v| {
                    v.owner.is_accessible_to(
                        viewer,
                        viewer_household,
                        viewer_settlement,
                        viewer_faction,
                    )
                })
                .filter(|v| !require_reachable || is_reachable(v.tile))
                .min_by_key(|v| dist(v.tile) + claim_penalty(v.tile))
                .map(|v| v.tile)
        };
        pick(true).or_else(|| pick(false))
    }

    /// Pick the nearest visible entity-anchored target (`entity == Some`) of
    /// the requested kind, excluding entries on storage tiles (so an agent
    /// doesn't try to "scavenge" their own deposit). Used as the vision-first
    /// short-circuit for scavenge methods.
    pub fn nearest_scavenge_target(
        &self,
        kind: MemoryKind,
        dist: impl Fn((i32, i32)) -> i32,
        is_storage_tile: impl Fn((i32, i32)) -> bool,
        is_reachable: impl Fn((i32, i32)) -> bool,
    ) -> Option<(Entity, (i32, i32))> {
        // Phase 2a: prefer reachable scavenge targets; fall back to the
        // unfiltered nearest if everything visible is in a disconnected
        // chunk. Matches `nearest_gather_target`'s two-pass shape.
        let pick = |require_reachable: bool| -> Option<(Entity, (i32, i32))> {
            self.iter_kind(kind)
                .filter_map(|v| v.entity.map(|e| (e, v.tile)))
                .filter(|(_, tile)| !is_storage_tile(*tile))
                .filter(|(_, tile)| !require_reachable || is_reachable(*tile))
                .min_by_key(|(_, tile)| dist(*tile))
        };
        pick(true).or_else(|| pick(false))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RelEntry {
    pub entity: Entity,
    pub affinity: i8,
    pub age: u8,
}

#[derive(Component, Clone, Default)]
pub struct RelationshipMemory {
    pub entries: [Option<RelEntry>; 16],
}

impl RelationshipMemory {
    pub fn update(&mut self, entity: Entity, delta: i8) {
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.entity == entity {
                    e.affinity = e.affinity.saturating_add(delta);
                    e.age = 0;
                    return;
                }
            }
        }
        for slot in &mut self.entries {
            if slot.is_none() {
                *slot = Some(RelEntry {
                    entity,
                    affinity: delta,
                    age: 0,
                });
                return;
            }
        }
        // Evict lowest |affinity|
        let mut min_idx = 0usize;
        let mut min_abs = u8::MAX;
        for (i, slot) in self.entries.iter().enumerate() {
            if let Some(e) = slot {
                let abs = e.affinity.unsigned_abs();
                if abs < min_abs {
                    min_abs = abs;
                    min_idx = i;
                }
            }
        }
        self.entries[min_idx] = Some(RelEntry {
            entity,
            affinity: delta,
            age: 0,
        });
    }

    pub fn get_affinity(&self, entity: Entity) -> i8 {
        for slot in &self.entries {
            if let Some(e) = slot {
                if e.entity == entity {
                    return e.affinity;
                }
            }
        }
        0
    }

    pub fn decay(&mut self) {
        for slot in &mut self.entries {
            if let Some(e) = slot {
                e.age = e.age.saturating_add(1);
                let threshold = e.age / 10;
                if e.affinity.unsigned_abs() <= threshold {
                    if e.affinity > 0 {
                        e.affinity -= 1;
                    } else if e.affinity < 0 {
                        e.affinity += 1;
                    }
                }
                if e.affinity == 0 && e.age > 50 {
                    *slot = None;
                }
            }
        }
    }
}

/// Daily decay tick for `RelationshipMemory`. The companion tile-memory
/// decay was retired with the 32-slot ring in Phase 7 — `SharedKnowledge`
/// owns cluster freshness via `cluster_decay_system` now.
pub fn relationship_decay_system(clock: Res<SimClock>, mut query: Query<&mut RelationshipMemory>) {
    if clock.tick % crate::world::seasons::TICKS_PER_DAY as u64 != 0 {
        return;
    }
    for mut rel in query.iter_mut() {
        rel.decay();
    }
}

/// Round-robin cursor over Persons for the per-tick vision cap. Together
/// with the existing bucket gate, this ensures a mass-movement tick (e.g.
/// every Person crossing a tile boundary at once) cannot spike vision-scan
/// cost above `PerfWorkBudget::vision_recomputes_per_tick`.
#[derive(Resource, Default)]
pub struct VisionCursor {
    pub next_entity_bits: u64,
}

pub fn vision_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    budget: Res<crate::simulation::perf::PerfWorkBudget>,
    mut cursor: ResMut<VisionCursor>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    plant_query: Query<(
        &crate::simulation::plants::Plant,
        Option<&crate::simulation::shared_knowledge::LandClaim>,
        Option<&crate::simulation::plants::PlantSpecies>,
    )>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    prey_query: Query<
        (
            Entity,
            &crate::simulation::combat::Health,
            Has<crate::simulation::animals::Wolf>,
            Has<crate::simulation::animals::Deer>,
            Has<crate::simulation::animals::Horse>,
            Has<crate::simulation::animals::Cow>,
        ),
        Or<(
            With<crate::simulation::animals::Wolf>,
            With<crate::simulation::animals::Deer>,
            With<crate::simulation::animals::Horse>,
            With<crate::simulation::animals::Cow>,
        )>,
    >,
    mut shared: ResMut<crate::simulation::shared_knowledge::SharedKnowledge>,
    mut query: Query<
        (
            Entity,
            &Transform,
            &BucketSlot,
            &LodLevel,
            &crate::simulation::person::PersonAI,
            Option<&crate::simulation::faction::FactionMember>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            Option<&crate::simulation::vision::ActiveLookout>,
            &mut CurrentVision,
        ),
        With<Person>,
    >,
    timings: Res<crate::simulation::speed::SuspectSystemTimings>,
) {
    use crate::simulation::shared_knowledge::KnowledgeTier;

    let _t = timings.guard(crate::simulation::speed::suspect::VISION);
    let now = clock.tick;

    // Phase 3.1: per-tick vision cap. Bucket gating (`clock.is_active(slot)`)
    // already amortises across `population/bucket_size` ticks; this cap is
    // the safety net against mass-movement bursts (every Person crossing
    // a tile boundary at once shouldn't spike vision-scan cost above
    // `budget.vision_recomputes_per_tick`).
    //
    // Build the eligible-this-tick set, sort by entity bits, rotate by
    // cursor pivot, take cap. Pure round-robin with no per-tick gap.
    let cap = budget.vision_recomputes_per_tick.max(1);
    let mut eligible: Vec<Entity> = Vec::new();
    for (entity, _, slot, lod, _, _, _, _, _) in query.iter() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        eligible.push(entity);
    }
    let allowed: ahash::AHashSet<Entity> = if eligible.is_empty() {
        ahash::AHashSet::default()
    } else {
        eligible.sort_unstable_by_key(|e| e.to_bits());
        let pivot = eligible
            .iter()
            .position(|e| e.to_bits() >= cursor.next_entity_bits)
            .unwrap_or(0);
        let take = cap.min(eligible.len());
        let slice: ahash::AHashSet<Entity> = (0..take)
            .map(|offset| eligible[(pivot + offset) % eligible.len()])
            .collect();
        if let Some(&last) = (0..take)
            .map(|offset| &eligible[(pivot + offset) % eligible.len()])
            .last()
        {
            cursor.next_entity_bits = last.to_bits().saturating_add(1);
        }
        slice
    };

    for (entity, transform, slot, lod, ai, faction_member, household_member, active_lookout, mut current_vision) in
        query.iter_mut()
    {
        let view_radius = crate::simulation::vision::effective_vision_radius(active_lookout) as i32;
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if !allowed.contains(&entity) {
            continue;
        }
        // Refresh the per-agent buffer every time the bucket fires. Dispatchers
        // read this slot in the same tick (ParallelB after vision's pass) and
        // short-circuit `SharedKnowledge` when a matching entry exists.
        current_vision.entries.clear();

        // The agent writes to its *finest* tier — Household if a member,
        // otherwise Faction. Settlement and faction-wide knowledge are
        // populated through `cluster_tier_promotion_system` when officials
        // socialise. SOLO agents (faction 0) write to and read from the
        // SOLO faction tier.
        let write_tier = if let Some(hm) = household_member {
            KnowledgeTier::Household(hm.household_id)
        } else if let Some(fm) = faction_member {
            KnowledgeTier::Faction(fm.faction_id)
        } else {
            KnowledgeTier::Faction(0)
        };

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let from_z = ai.current_z;

        for dy in -view_radius..=view_radius {
            for dx in -view_radius..=view_radius {
                if dx * dx + dy * dy > view_radius * view_radius {
                    continue;
                }

                let ntx = tx + dx;
                let nty = ty + dy;
                let to_z = chunk_map.surface_z_at(ntx, nty) as i8;

                if !crate::simulation::line_of_sight::has_los(
                    &chunk_map,
                    &door_map,
                    (tx, ty, from_z),
                    (ntx, nty, to_z),
                ) {
                    continue;
                }

                if let Some(&entity) = plant_map.0.get(&(ntx, nty)) {
                    if let Ok((plant, land_claim, species_opt)) = plant_query.get(entity) {
                        if plant.stage == crate::simulation::plants::GrowthStage::Mature {
                            let owner = land_claim
                                .map(|lc| lc.owner)
                                .unwrap_or(ResourceOwner::Public);
                            // Phase 3 (biome-native plants): seed every
                            // memory channel the plant's harvest profiles
                            // produce — fiber/medicine/dye/resin/latex/
                            // oilseed in addition to the legacy
                            // AnyEdible/wood lenses. Multi-profile species
                            // (e.g. oak: fruit + wood) tag both channels in
                            // one sweep so independent HTN gather methods
                            // can find the same plant.
                            let kinds = crate::simulation::plants::plant_memory_kinds(
                                species_opt.map(|s| s.id()),
                                plant.kind,
                            );
                            for k in kinds {
                                shared.report_sighting(
                                    write_tier,
                                    (ntx, nty),
                                    k,
                                    owner,
                                    now,
                                );
                                current_vision.entries.push(VisionEntry {
                                    kind: k,
                                    tile: (ntx, nty),
                                    entity: None,
                                    owner,
                                });
                            }
                        }
                    }
                }

                let catalog = core_ids::catalog();
                for &entity in spatial.get(ntx, nty) {
                    if let Ok(item) = item_query.get(entity) {
                        let resource_id = item.item.resource_id;
                        // SharedKnowledge keeps the legacy class filter (Food
                        // canonicalises to AnyEdible; Material/Seed clusters
                        // by Resource(rid); other classes don't seed
                        // clusters). CurrentVision is broader — every visible
                        // GroundItem the agent could harvest under a chief
                        // Stockpile{rid} posting goes in by Resource(rid),
                        // matching the old in-dispatcher SpatialIndex scan
                        // that filtered on `gi.item.resource_id == target_rid`
                        // regardless of class.
                        let shared_kind =
                            catalog.get(resource_id).and_then(|def| match def.class {
                                crate::economy::resource_catalog::ResourceClass::Food => {
                                    Some(MemoryKind::AnyEdible)
                                }
                                crate::economy::resource_catalog::ResourceClass::Material
                                | crate::economy::resource_catalog::ResourceClass::Seed => {
                                    Some(MemoryKind::Resource(resource_id))
                                }
                                _ => None,
                            });
                        if let Some(k) = shared_kind {
                            // Loose ground items belong to no one — Public.
                            shared.report_sighting(
                                write_tier,
                                (ntx, nty),
                                k,
                                ResourceOwner::Public,
                                now,
                            );
                        }
                        // Vision-first: dispatchers look up entries by
                        // MemoryKind. Push Resource(rid) for every item, plus
                        // an extra AnyEdible row for foods so the food
                        // dispatchers can iterate them by class without
                        // knowing the specific resource id.
                        current_vision.entries.push(VisionEntry {
                            kind: MemoryKind::Resource(resource_id),
                            tile: (ntx, nty),
                            entity: Some(entity),
                            owner: ResourceOwner::Public,
                        });
                        if shared_kind == Some(MemoryKind::AnyEdible) {
                            current_vision.entries.push(VisionEntry {
                                kind: MemoryKind::AnyEdible,
                                tile: (ntx, nty),
                                entity: Some(entity),
                                owner: ResourceOwner::Public,
                            });
                        }
                    } else if let Ok((_, health, is_wolf, is_deer, is_horse, is_cow)) =
                        prey_query.get(entity)
                    {
                        if !health.is_dead() {
                            // Prey: predators + game (Wolf / Deer) — feeds the
                            // hunting pipeline.
                            if is_wolf || is_deer {
                                shared.report_sighting(
                                    write_tier,
                                    (ntx, nty),
                                    MemoryKind::Prey,
                                    ResourceOwner::Public,
                                    now,
                                );
                                current_vision.entries.push(VisionEntry {
                                    kind: MemoryKind::Prey,
                                    tile: (ntx, nty),
                                    entity: Some(entity),
                                    owner: ResourceOwner::Public,
                                });
                            }
                            // Herd sighting: knowledge-gated grazing signal for
                            // nomad migration scoring (Deer / Horse / Cattle).
                            if is_deer || is_horse || is_cow {
                                shared.report_sighting(
                                    write_tier,
                                    (ntx, nty),
                                    MemoryKind::HerdSighting,
                                    ResourceOwner::Public,
                                    now,
                                );
                                current_vision.entries.push(VisionEntry {
                                    kind: MemoryKind::HerdSighting,
                                    tile: (ntx, nty),
                                    entity: Some(entity),
                                    owner: ResourceOwner::Public,
                                });
                            }
                        }
                    }
                }

                if let Some(tile_kind) = chunk_map.tile_kind_at(ntx, nty) {
                    if tile_kind == crate::world::tile::TileKind::Stone {
                        shared.report_sighting(
                            write_tier,
                            (ntx, nty),
                            MemoryKind::stone(),
                            ResourceOwner::Public,
                            now,
                        );
                        current_vision.entries.push(VisionEntry {
                            kind: MemoryKind::stone(),
                            tile: (ntx, nty),
                            entity: None,
                            owner: ResourceOwner::Public,
                        });
                    }
                    // Vision is additive only — it never depletes. Stone
                    // cluster shrinkage fires on gather-arrival
                    // (`gather.rs` `invalidate_tile_across_tier_set`), never
                    // here. The old `else { report_depleted(stone) }` ran an
                    // O(local-cluster-count) `cluster_at` scan on ~every
                    // non-Stone tile in the view disc — the dominant
                    // (cluster-density-scaling) cost of `vision_system`.
                    // Phase F.2 — Marsh tiles carry harvestable reed beds.
                    // Report as `MemoryKind::reeds()` so the standard
                    // AcquireGood / Stockpile gather chain (
                    // `GatherFromKnownMethod` → `htn_acquire_good_dispatch_system`
                    // → `Task::Gather { tile }` → `gather_system` Marsh
                    // branch) can route a worker without bespoke
                    // dispatch infrastructure.
                    if tile_kind == crate::world::tile::TileKind::Marsh {
                        shared.report_sighting(
                            write_tier,
                            (ntx, nty),
                            MemoryKind::reeds(),
                            ResourceOwner::Public,
                            now,
                        );
                        current_vision.entries.push(VisionEntry {
                            kind: MemoryKind::reeds(),
                            tile: (ntx, nty),
                            entity: None,
                            owner: ResourceOwner::Public,
                        });
                    }
                }
            }
        }
    }
}

/// Per-tick affinity gain from a *deliberate* Socialize interaction.
pub const DEDICATED_AFFINITY_STEP: i8 = 5;
/// Per-tick affinity gain from *ambient* work-proximity contact. Slow:
/// acquaintance, not courtship.
pub const AMBIENT_AFFINITY_STEP: i8 = 1;
/// Ceiling that ambient-only contact can raise affinity to. Must stay
/// **below** the cohabitation/bed-reassignment thresholds in
/// `construction.rs` (`PARTNER_AFFINITY_THRESHOLD = 60`,
/// `REASSIGN_AFFINITY_THRESHOLD = 80`) so working near someone forms an
/// acquaintance but never, by itself, a "move in together" bond — that
/// requires deliberate Socialize. (Daily `relationship_decay` is far too
/// weak to bound monotonic per-tick accrual on its own, so the cap, not a
/// reduced rate alone, is what keeps ambient bonds sub-courtship.)
pub const AMBIENT_AFFINITY_CAP: i8 = 40;

/// Increment relationship affinity between socializing agents within 3
/// tiles. The MemoryEntry tile-gossip half of this system was retired in
/// Phase 7 — `cluster_tier_promotion_system` (knowledge.rs) now bubbles
/// `SharedKnowledge` clusters between household / settlement / faction
/// tiers when constituents talk to officials.
pub fn conversation_memory_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    mut query: Query<(
        Entity,
        &AgentGoal,
        &Transform,
        &mut RelationshipMemory,
        &LodLevel,
        Option<&SecondarySocial>,
    )>,
) {
    // Two-tier bonding (decision: reduced ambient rate). A *deliberate*
    // Socialize pair bonds fast (+5/tick, uncapped → can reach the
    // cohabitation thresholds). A purely *ambient* work-proximity pair
    // bonds slowly (+1/tick) and only up to `AMBIENT_AFFINITY_CAP` — an
    // acquaintance ceiling below courtship, so coworkers don't auto-move-in
    // together. `dedicated` = on explicit `AgentGoal::Socialize`; a pair
    // counts as dedicated if *either* end chose to socialize.
    let now = clock.tick as u32;
    let mut socializers: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut dedicated: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for (e, goal, _, _, lod, sec) in query.iter() {
        if !is_social_contact(*goal, *lod, sec, now) {
            continue;
        }
        socializers.insert(e);
        if matches!(goal, AgentGoal::Socialize) && *lod != LodLevel::Dormant {
            dedicated.insert(e);
        }
    }

    if socializers.is_empty() {
        return;
    }

    for (entity, goal, transform, mut rel, lod, sec) in query.iter_mut() {
        if !is_social_contact(*goal, *lod, sec, now) {
            continue;
        }
        let self_dedicated = dedicated.contains(&entity);
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    if !socializers.contains(&other) {
                        continue;
                    }
                    if self_dedicated || dedicated.contains(&other) {
                        // Chosen social time → fast, uncapped bonding.
                        rel.update(other, DEDICATED_AFFINITY_STEP);
                    } else {
                        // Ambient work proximity → slow, capped at the
                        // acquaintance ceiling (never raises an already
                        // higher bond, never pushes past the cap).
                        let cur = rel.get_affinity(other);
                        if cur < AMBIENT_AFFINITY_CAP {
                            let step = AMBIENT_AFFINITY_STEP.min(AMBIENT_AFFINITY_CAP - cur);
                            rel.update(other, step);
                        }
                    }
                }
            }
        }
    }
}
