use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::person::Person;
use super::schedule::{BucketSlot, SimClock};
use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use ahash::AHashMap;
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
    /// Used by readers that filter on "is the remembered tile a food
    /// source?" — e.g. plan/mod.rs's plant-tile fallback (Food/Wood require
    /// a live plant; Stone tolerates the plant being missing).
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

#[derive(Clone, Copy, Debug)]
pub struct MemoryEntry {
    pub tile: (i32, i32),
    pub kind: MemoryKind,
    pub entity: Option<Entity>,
    pub freshness: u8,
}

#[derive(Component, Clone, Default)]
pub struct AgentMemory {
    pub entries: [Option<MemoryEntry>; 32],
    /// Pluralist Economy R8 — region-aware memory. Tracks
    /// `(SettlementId, freshness)` for up to 8 settlements the
    /// agent has visited or heard about (gossip propagation in
    /// `awareness_gossip_system` extension lands as a follow-on).
    /// **Additive**: the existing 32-entry tile array is unchanged
    /// — `gather_target_tile` / `scavenge_target_*` still drive
    /// every existing dispatcher. `visited_settlements` only
    /// informs region-aware queries (Trader R10's
    /// `ParticipateInMarket` walks pairs in this slot).
    pub visited_settlements:
        [Option<(crate::simulation::settlement::SettlementId, u8)>; 8],
}

impl AgentMemory {
    fn insert_with_freshness(
        &mut self,
        tile: (i32, i32),
        kind: MemoryKind,
        entity: Option<Entity>,
        freshness: u8,
    ) {
        // Update same tile+kind+entity entry
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.tile == tile && e.kind == kind && e.entity == entity {
                    if freshness > e.freshness {
                        e.freshness = freshness;
                    }
                    return;
                }
            }
        }
        // Find empty slot
        for slot in &mut self.entries {
            if slot.is_none() {
                *slot = Some(MemoryEntry {
                    tile,
                    kind,
                    entity,
                    freshness,
                });
                return;
            }
        }
        // Evict lowest freshness if ours is higher
        let mut min_idx = 0usize;
        let mut min_fresh = u8::MAX;
        for (i, slot) in self.entries.iter().enumerate() {
            if let Some(e) = slot {
                if e.freshness < min_fresh {
                    min_fresh = e.freshness;
                    min_idx = i;
                }
            }
        }
        if freshness > min_fresh {
            self.entries[min_idx] = Some(MemoryEntry {
                tile,
                kind,
                entity,
                freshness,
            });
        }
    }

    pub fn record(&mut self, tile: (i32, i32), kind: MemoryKind) {
        self.insert_with_freshness(tile, kind, None, 255);
    }

    pub fn record_entity(&mut self, tile: (i32, i32), kind: MemoryKind, entity: Entity) {
        self.insert_with_freshness(tile, kind, Some(entity), 255);
    }

    /// Pluralist Economy R8: record a visited / heard-about
    /// settlement. Idempotent — re-recording the same id resets the
    /// freshness to 255. When the slot ring is full, evicts the
    /// lowest-freshness entry (mirrors the tile-ring eviction
    /// pattern in `insert_with_freshness`).
    pub fn record_settlement(
        &mut self,
        id: crate::simulation::settlement::SettlementId,
    ) {
        // Update existing entry.
        for slot in &mut self.visited_settlements {
            if let Some((existing, fresh)) = slot {
                if *existing == id {
                    *fresh = 255;
                    return;
                }
            }
        }
        // Find empty slot.
        for slot in &mut self.visited_settlements {
            if slot.is_none() {
                *slot = Some((id, 255));
                return;
            }
        }
        // Evict the lowest-freshness slot.
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

    // Bug 4 fix: remove ALL matching entries, not just the first one.
    pub fn forget(&mut self, tile: (i32, i32), kind: MemoryKind) {
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.tile == tile && e.kind == kind {
                    *slot = None;
                }
            }
        }
    }

    pub fn best_for(&self, kind: MemoryKind) -> Option<(i32, i32)> {
        let mut best_fresh = 0u8;
        let mut best_tile = None;
        for slot in &self.entries {
            if let Some(e) = slot {
                if e.kind == kind && e.freshness > best_fresh {
                    best_fresh = e.freshness;
                    best_tile = Some(e.tile);
                }
            }
        }
        best_tile
    }

    pub fn best_for_dist_weighted(
        &self,
        kind: MemoryKind,
        from_pos: (i32, i32),
    ) -> Option<(i32, i32)> {
        let mut best_score = -1.0f32;
        let mut best_tile = None;
        for slot in &self.entries {
            if let Some(e) = slot {
                if e.kind == kind {
                    let dx = (e.tile.0 as i32 - from_pos.0).abs();
                    let dy = (e.tile.1 as i32 - from_pos.1).abs();
                    let dist = (dx + dy).max(1) as f32;
                    // Score = freshness / dist
                    let score = e.freshness as f32 / dist;
                    if score > best_score {
                        best_score = score;
                        best_tile = Some(e.tile);
                    }
                }
            }
        }
        best_tile
    }

    pub fn best_entity_for_dist_weighted(
        &self,
        kind: MemoryKind,
        from_pos: (i32, i32),
    ) -> Option<(Entity, i32, i32)> {
        let mut best_score = -1.0f32;
        let mut best_res = None;
        for slot in &self.entries {
            if let Some(e) = slot {
                if e.kind == kind && e.entity.is_some() {
                    let dx = (e.tile.0 as i32 - from_pos.0).abs();
                    let dy = (e.tile.1 as i32 - from_pos.1).abs();
                    let dist = (dx + dy).max(1) as f32;
                    let score = e.freshness as f32 / dist;
                    if score > best_score {
                        best_score = score;
                        best_res = Some((e.entity.unwrap(), e.tile.0, e.tile.1));
                    }
                }
            }
        }
        best_res
    }

    pub fn best_entity_for_dist_weighted_filtered<F: Fn(Entity, (i32, i32)) -> bool>(
        &self,
        kind: MemoryKind,
        from_pos: (i32, i32),
        accept: F,
    ) -> Option<(Entity, i32, i32)> {
        let mut best_score = -1.0f32;
        let mut best_res = None;
        for slot in &self.entries {
            if let Some(e) = slot {
                if e.kind == kind {
                    if let Some(ent) = e.entity {
                        if !accept(ent, e.tile) {
                            continue;
                        }
                        let dx = (e.tile.0 as i32 - from_pos.0).abs();
                        let dy = (e.tile.1 as i32 - from_pos.1).abs();
                        let dist = (dx + dy).max(1) as f32;
                        let score = e.freshness as f32 / dist;
                        if score > best_score {
                            best_score = score;
                            best_res = Some((ent, e.tile.0, e.tile.1));
                        }
                    }
                }
            }
        }
        best_res
    }

    pub fn decay(&mut self) {
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.freshness == 0 {
                    *slot = None;
                } else {
                    e.freshness -= 1;
                }
            }
        }
    }

    pub fn top_entries(&self, n: usize) -> Vec<MemoryEntry> {
        let mut entries: Vec<MemoryEntry> = self.entries.iter().filter_map(|s| *s).collect();
        entries.sort_unstable_by(|a, b| b.freshness.cmp(&a.freshness));
        entries.truncate(n);
        entries
    }

    pub fn receive_gossip(&mut self, entry: MemoryEntry) {
        self.insert_with_freshness(entry.tile, entry.kind, entry.entity, entry.freshness / 2);
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

pub fn memory_decay_system(
    clock: Res<SimClock>,
    mut query: Query<(&mut AgentMemory, &mut RelationshipMemory)>,
) {
    if clock.tick % 3600 != 0 {
        return;
    }
    for (mut memory, mut rel) in query.iter_mut() {
        memory.decay();
        rel.decay();
    }
}

pub fn vision_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    plant_query: Query<&crate::simulation::plants::Plant>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    prey_query: Query<
        (Entity, &crate::simulation::combat::Health),
        Or<(
            With<crate::simulation::animals::Wolf>,
            With<crate::simulation::animals::Deer>,
        )>,
    >,
    mut query: Query<
        (
            &Transform,
            &mut AgentMemory,
            &BucketSlot,
            &LodLevel,
            &crate::simulation::person::PersonAI,
        ),
        With<Person>,
    >,
) {
    const VIEW_RADIUS: i32 = 15;

    for (transform, mut memory, slot, lod, ai) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

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

                // Check plants — Bug 1 fix: record with entity so target_entity is set on
                // dispatch, allowing goal_update_system to validate via the plant query
                // instead of falling through to the stone tile check.
                if let Some(&entity) = plant_map.0.get(&(ntx, nty)) {
                    if let Ok(plant) = plant_query.get(entity) {
                        let kind = match plant.kind {
                            crate::simulation::plants::PlantKind::BerryBush
                            | crate::simulation::plants::PlantKind::Grain => {
                                MemoryKind::AnyEdible
                            }
                            crate::simulation::plants::PlantKind::Tree => MemoryKind::wood(),
                        };
                        if plant.stage == crate::simulation::plants::GrowthStage::Mature {
                            memory.record_entity((ntx as i32, nty as i32), kind, entity);
                        } else {
                            memory.forget((ntx as i32, nty as i32), kind);
                        }
                    }
                } else {
                    memory.forget((ntx as i32, nty as i32), MemoryKind::AnyEdible);
                    memory.forget((ntx as i32, nty as i32), MemoryKind::wood());
                }

                // Check spatial for entities (items, prey).
                //
                // Sub-PR 2: derive the memory variant from the catalog rather
                // than switching on legacy `Good` variants. Edibles (every
                // resource whose `class == Food`) collapse into the
                // `AnyEdible` aggregate so AcquireFood / StockpileFood /
                // Forage readers don't have to enumerate Fruit/Meat/Grain.
                // Materials (Wood / Stone today, Iron / Copper / Tin / any
                // future ore for free) and Seeds record the specific
                // `Resource(id)` so AcquireGood-style readers ask for one
                // concrete resource. Other classes (Tool / Weapon / Armor /
                // Cloth / Hide / Luxury / Currency / Knowledge / Fuel) skip
                // the write — no current gather/scavenge consumer, and
                // recording them would churn the 32-slot memory cap. Adding
                // a class here is the single touch-point if a future method
                // wants those resources remembered.
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
                            memory.record_entity((ntx as i32, nty as i32), k, entity);
                        }
                    } else if let Ok((e, health)) = prey_query.get(entity) {
                        if !health.is_dead() {
                            memory.record_entity((ntx as i32, nty as i32), MemoryKind::Prey, e);
                        }
                    }
                }

                // Check tile kinds (stone fallback)
                if let Some(tile_kind) = chunk_map.tile_kind_at(ntx, nty) {
                    if tile_kind == crate::world::tile::TileKind::Stone {
                        memory.record((ntx as i32, nty as i32), MemoryKind::stone());
                    } else {
                        memory.forget((ntx as i32, nty as i32), MemoryKind::stone());
                    }
                }
            }
        }
    }
}

pub fn conversation_memory_system(
    spatial: Res<SpatialIndex>,
    mut query: Query<(
        Entity,
        &AgentGoal,
        &Transform,
        &mut AgentMemory,
        &mut RelationshipMemory,
        &LodLevel,
    )>,
) {
    // Pass 1: collect memory snapshots from all Socialize agents (immutable borrow, dropped after collect)
    let snapshots: AHashMap<Entity, Vec<MemoryEntry>> = query
        .iter()
        .filter(|(_, goal, ..)| matches!(goal, AgentGoal::Socialize))
        .filter(|(_, _, _, _, _, lod)| **lod != LodLevel::Dormant)
        .map(|(e, _, _, mem, _, _)| (e, mem.top_entries(8)))
        .collect();

    if snapshots.is_empty() {
        return;
    }

    // Pass 2: apply gossip to each Socialize agent from nearby agents
    for (entity, goal, transform, mut memory, mut rel, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(goal, AgentGoal::Socialize) {
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
                    if let Some(entries) = snapshots.get(&other) {
                        for &entry in entries {
                            memory.receive_gossip(entry);
                        }
                        rel.update(other, 5);
                    }
                }
            }
        }
    }
}
