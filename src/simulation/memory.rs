use bevy::prelude::*;
use ahash::AHashMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::chunk::ChunkMap;
use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::schedule::{BucketSlot, SimClock};
use super::person::Person;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    Food  = 0,
    Wood  = 1,
    Stone = 2,
    Seed  = 3,
    Prey  = 4,
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryEntry {
    pub tile:      (i16, i16),
    pub kind:      MemoryKind,
    pub entity:    Option<Entity>,
    pub freshness: u8,
}

#[derive(Component, Clone, Default)]
pub struct AgentMemory {
    pub entries: [Option<MemoryEntry>; 32],
}

impl AgentMemory {
    fn insert_with_freshness(&mut self, tile: (i16, i16), kind: MemoryKind, entity: Option<Entity>, freshness: u8) {
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
                *slot = Some(MemoryEntry { tile, kind, entity, freshness });
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
            self.entries[min_idx] = Some(MemoryEntry { tile, kind, entity, freshness });
        }
    }

    pub fn record(&mut self, tile: (i16, i16), kind: MemoryKind) {
        self.insert_with_freshness(tile, kind, None, 255);
    }

    pub fn record_entity(&mut self, tile: (i16, i16), kind: MemoryKind, entity: Entity) {
        self.insert_with_freshness(tile, kind, Some(entity), 255);
    }

    pub fn forget(&mut self, tile: (i16, i16), kind: MemoryKind) {
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.tile == tile && e.kind == kind {
                    *slot = None;
                    return;
                }
            }
        }
    }

    pub fn best_for(&self, kind: MemoryKind) -> Option<(i16, i16)> {
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

    pub fn best_for_dist_weighted(&self, kind: MemoryKind, from_pos: (i32, i32)) -> Option<(i16, i16)> {
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

    pub fn best_entity_for_dist_weighted(&self, kind: MemoryKind, from_pos: (i32, i32)) -> Option<(Entity, i16, i16)> {
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
        let mut entries: Vec<MemoryEntry> = self.entries.iter()
            .filter_map(|s| *s)
            .collect();
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
    pub entity:   Entity,
    pub affinity: i8,
    pub age:      u8,
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
                *slot = Some(RelEntry { entity, affinity: delta, age: 0 });
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
        self.entries[min_idx] = Some(RelEntry { entity, affinity: delta, age: 0 });
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
                    if e.affinity > 0 { e.affinity -= 1; }
                    else if e.affinity < 0 { e.affinity += 1; }
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
    if clock.tick % 60 != 0 { return; }
    for (mut memory, mut rel) in query.iter_mut() {
        memory.decay();
        rel.decay();
    }
}

pub fn vision_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    plant_query: Query<&crate::simulation::plants::Plant>,
    item_query: Query<&crate::simulation::items::GroundItem>,
    prey_query: Query<(Entity, &crate::simulation::combat::Health), Or<(With<crate::simulation::animals::Wolf>, With<crate::simulation::animals::Deer>)>>,
    mut query: Query<(&Transform, &mut AgentMemory, &BucketSlot, &LodLevel), With<Person>>,
) {
    const VIEW_RADIUS: i32 = 15;

    for (transform, mut memory, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        for dy in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx*dx + dy*dy > VIEW_RADIUS*VIEW_RADIUS { continue; }
                
                let ntx = tx + dx;
                let nty = ty + dy;

                if !crate::simulation::line_of_sight::has_los(&chunk_map, (tx, ty), (ntx, nty)) {
                    continue;
                }

                // Check plants
                if let Some(&entity) = plant_map.0.get(&(ntx, nty)) {
                    if let Ok(plant) = plant_query.get(entity) {
                        let kind = match plant.kind {
                            crate::simulation::plants::PlantKind::FruitBush | crate::simulation::plants::PlantKind::Grain => MemoryKind::Food,
                            crate::simulation::plants::PlantKind::Tree => MemoryKind::Wood,
                        };
                        if plant.stage == crate::simulation::plants::GrowthStage::Mature {
                            memory.record((ntx as i16, nty as i16), kind);
                        } else {
                            memory.forget((ntx as i16, nty as i16), kind);
                        }
                    }
                } else {
                    memory.forget((ntx as i16, nty as i16), MemoryKind::Food);
                    memory.forget((ntx as i16, nty as i16), MemoryKind::Wood);
                }

                // Check spatial for entities (items, prey)
                for &entity in spatial.get(ntx, nty) {
                    if let Ok(item) = item_query.get(entity) {
                        let kind = match item.item.good {
                            crate::economy::goods::Good::Food => Some(MemoryKind::Food),
                            crate::economy::goods::Good::Wood => Some(MemoryKind::Wood),
                            crate::economy::goods::Good::Stone => Some(MemoryKind::Stone),
                            crate::economy::goods::Good::Seed => Some(MemoryKind::Seed),
                            _ => None,
                        };
                        if let Some(k) = kind {
                            memory.record_entity((ntx as i16, nty as i16), k, entity);
                        }
                    } else if let Ok((e, health)) = prey_query.get(entity) {
                        if !health.is_dead() {
                            memory.record_entity((ntx as i16, nty as i16), MemoryKind::Prey, e);
                        }
                    }
                }

                // Check tile kinds (stone fallback)
                if let Some(tile_kind) = chunk_map.tile_kind_at(ntx, nty) {
                    if tile_kind == crate::world::tile::TileKind::Stone {
                        memory.record((ntx as i16, nty as i16), MemoryKind::Stone);
                    } else {
                        memory.forget((ntx as i16, nty as i16), MemoryKind::Stone);
                    }
                }
            }
        }
    }
}

pub fn conversation_memory_system(
    spatial: Res<SpatialIndex>,
    mut query: Query<(Entity, &AgentGoal, &Transform, &mut AgentMemory, &mut RelationshipMemory, &LodLevel)>,
) {
    // Pass 1: collect memory snapshots from all Socialize agents (immutable borrow, dropped after collect)
    let snapshots: AHashMap<Entity, Vec<MemoryEntry>> = query.iter()
        .filter(|(_, goal, ..)| matches!(goal, AgentGoal::Socialize))
        .filter(|(_, _, _, _, _, lod)| **lod != LodLevel::Dormant)
        .map(|(e, _, _, mem, _, _)| (e, mem.top_entries(8)))
        .collect();

    if snapshots.is_empty() { return; }

    // Pass 2: apply gossip to each Socialize agent from nearby agents
    for (entity, goal, transform, mut memory, mut rel, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant { continue; }
        if !matches!(goal, AgentGoal::Socialize) { continue; }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity { continue; }
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
