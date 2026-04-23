use bevy::prelude::*;
use ahash::AHashMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::schedule::SimClock;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    Food  = 0,
    Wood  = 1,
    Stone = 2,
    Seed  = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryEntry {
    pub tile:      (i16, i16),
    pub kind:      MemoryKind,
    pub freshness: u8,
}

#[derive(Component, Clone, Default)]
pub struct AgentMemory {
    pub entries: [Option<MemoryEntry>; 16],
}

impl AgentMemory {
    fn insert_with_freshness(&mut self, tile: (i16, i16), kind: MemoryKind, freshness: u8) {
        // Update same tile+kind entry
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.tile == tile && e.kind == kind {
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
                *slot = Some(MemoryEntry { tile, kind, freshness });
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
            self.entries[min_idx] = Some(MemoryEntry { tile, kind, freshness });
        }
    }

    pub fn record(&mut self, tile: (i16, i16), kind: MemoryKind) {
        self.insert_with_freshness(tile, kind, 255);
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
        self.insert_with_freshness(entry.tile, entry.kind, entry.freshness / 2);
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
