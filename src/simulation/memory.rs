use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::person::Person;
use super::schedule::{BucketSlot, SimClock};
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
    pub visited_settlements:
        [Option<(crate::simulation::settlement::SettlementId, u8)>; 8],
}

impl AgentMemory {
    /// Pluralist Economy R8: record a visited / heard-about settlement.
    /// Idempotent — re-recording the same id resets the freshness to 255.
    /// When the slot ring is full, evicts the lowest-freshness entry.
    pub fn record_settlement(
        &mut self,
        id: crate::simulation::settlement::SettlementId,
    ) {
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
pub fn relationship_decay_system(
    clock: Res<SimClock>,
    mut query: Query<&mut RelationshipMemory>,
) {
    if clock.tick % 3600 != 0 {
        return;
    }
    for mut rel in query.iter_mut() {
        rel.decay();
    }
}

pub fn vision_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    plant_query: Query<(
        &crate::simulation::plants::Plant,
        Option<&crate::simulation::shared_knowledge::LandClaim>,
    )>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    prey_query: Query<
        (Entity, &crate::simulation::combat::Health),
        Or<(
            With<crate::simulation::animals::Wolf>,
            With<crate::simulation::animals::Deer>,
        )>,
    >,
    mut shared: ResMut<crate::simulation::shared_knowledge::SharedKnowledge>,
    query: Query<
        (
            &Transform,
            &BucketSlot,
            &LodLevel,
            &crate::simulation::person::PersonAI,
            Option<&crate::simulation::faction::FactionMember>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
        ),
        With<Person>,
    >,
) {
    use crate::simulation::shared_knowledge::{KnowledgeTier, ResourceOwner};
    const VIEW_RADIUS: i32 = 15;

    let now = clock.tick;
    for (transform, slot, lod, ai, faction_member, household_member) in query.iter() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

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

        for dy in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx * dx + dy * dy > VIEW_RADIUS * VIEW_RADIUS {
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
                    if let Ok((plant, land_claim)) = plant_query.get(entity) {
                        let kind = match plant.kind {
                            crate::simulation::plants::PlantKind::BerryBush
                            | crate::simulation::plants::PlantKind::Grain => {
                                MemoryKind::AnyEdible
                            }
                            crate::simulation::plants::PlantKind::Tree => MemoryKind::wood(),
                        };
                        if plant.stage == crate::simulation::plants::GrowthStage::Mature {
                            let owner = land_claim
                                .map(|lc| lc.owner)
                                .unwrap_or(ResourceOwner::Public);
                            shared.report_sighting(write_tier, (ntx, nty), kind, owner, now);
                        } else {
                            shared.report_depleted(write_tier, (ntx, nty), kind);
                        }
                    }
                } else {
                    shared.report_depleted(write_tier, (ntx, nty), MemoryKind::AnyEdible);
                    shared.report_depleted(write_tier, (ntx, nty), MemoryKind::wood());
                }

                let catalog = core_ids::catalog();
                for &entity in spatial.get(ntx, nty) {
                    if let Ok(item) = item_query.get(entity) {
                        let resource_id = item.item.resource_id;
                        let kind = catalog.get(resource_id).and_then(|def| match def.class {
                            crate::economy::resource_catalog::ResourceClass::Food => {
                                Some(MemoryKind::AnyEdible)
                            }
                            crate::economy::resource_catalog::ResourceClass::Material
                            | crate::economy::resource_catalog::ResourceClass::Seed => {
                                Some(MemoryKind::Resource(resource_id))
                            }
                            _ => None,
                        });
                        if let Some(k) = kind {
                            // Loose ground items belong to no one — Public.
                            shared.report_sighting(write_tier, (ntx, nty), k, ResourceOwner::Public, now);
                        }
                    } else if let Ok((_, health)) = prey_query.get(entity) {
                        if !health.is_dead() {
                            shared.report_sighting(write_tier, (ntx, nty), MemoryKind::Prey, ResourceOwner::Public, now);
                        }
                    }
                }

                if let Some(tile_kind) = chunk_map.tile_kind_at(ntx, nty) {
                    if tile_kind == crate::world::tile::TileKind::Stone {
                        shared.report_sighting(write_tier, (ntx, nty), MemoryKind::stone(), ResourceOwner::Public, now);
                    } else {
                        shared.report_depleted(write_tier, (ntx, nty), MemoryKind::stone());
                    }
                }
            }
        }
    }
}

/// Increment relationship affinity between socializing agents within 3
/// tiles. The MemoryEntry tile-gossip half of this system was retired in
/// Phase 7 — `cluster_tier_promotion_system` (knowledge.rs) now bubbles
/// `SharedKnowledge` clusters between household / settlement / faction
/// tiers when constituents talk to officials.
pub fn conversation_memory_system(
    spatial: Res<SpatialIndex>,
    mut query: Query<(
        Entity,
        &AgentGoal,
        &Transform,
        &mut RelationshipMemory,
        &LodLevel,
    )>,
) {
    // Collect socializer entities first so the borrow-checker accepts a
    // single mutable pass below.
    let socializers: ahash::AHashSet<Entity> = query
        .iter()
        .filter(|(_, goal, _, _, lod)| {
            matches!(goal, AgentGoal::Socialize) && **lod != LodLevel::Dormant
        })
        .map(|(e, _, _, _, _)| e)
        .collect();

    if socializers.is_empty() {
        return;
    }

    for (entity, goal, transform, mut rel, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !matches!(goal, AgentGoal::Socialize) {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    if socializers.contains(&other) {
                        rel.update(other, 5);
                    }
                }
            }
        }
    }
}
