use ahash::AHashMap;
use bevy::prelude::*;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use super::goals::{AgentGoal, Personality};
use super::memory::RelationshipMemory;
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;

pub const SOLO: u32 = 0;
pub const BOND_THRESHOLD: u8 = 180;
const CAMP_KEEP: u8 = 2;
const SOCIAL_RADIUS: i32 = 3;

#[derive(Component, Clone, Copy)]
pub struct FactionMember {
    pub faction_id:  u32,
    pub bond_target: Option<Entity>,
    pub bond_timer:  u8,
    /// Cooldown ticks after giving birth before reproduction need resets again.
    pub birth_cooldown: u32,
}

impl Default for FactionMember {
    fn default() -> Self {
        Self { faction_id: SOLO, bond_target: None, bond_timer: 0, birth_cooldown: 0 }
    }
}

pub struct FactionData {
    pub food_stock:   f32,
    pub home_tile:    (i16, i16),
    pub member_count: u32,
    pub raid_target:  Option<u32>,
    pub under_raid:   bool,
}

#[derive(Resource, Default)]
pub struct FactionRegistry {
    pub factions: AHashMap<u32, FactionData>,
    pub next_id:  u32,
}

impl FactionRegistry {
    pub fn create_faction(&mut self, home_tile: (i16, i16)) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        self.factions.insert(id, FactionData { food_stock: 0.0, home_tile, member_count: 0, raid_target: None, under_raid: false });
        id
    }

    pub fn add_member(&mut self, faction_id: u32) {
        if let Some(f) = self.factions.get_mut(&faction_id) {
            f.member_count += 1;
        }
    }

    pub fn remove_member(&mut self, faction_id: u32) {
        if let Some(f) = self.factions.get_mut(&faction_id) {
            f.member_count = f.member_count.saturating_sub(1);
        }
    }
}

// ── Bonding system ────────────────────────────────────────────────────────────

pub fn bonding_system(
    spatial: Res<SpatialIndex>,
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(Entity, &mut FactionMember, &Personality, &Transform)>,
    mut rel_query: Query<Option<&mut RelationshipMemory>>,
) {
    // Collect solo agents so we can iterate without borrow conflicts
    let solo_agents: Vec<(Entity, (i32, i32))> = query
        .iter()
        .filter_map(|(e, fm, _, transform)| {
            if fm.faction_id == SOLO {
                let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                Some((e, (tx, ty)))
            } else {
                None
            }
        })
        .collect();

    for (entity, (tx, ty)) in &solo_agents {
        // Find any adjacent entity
        let mut found_neighbor: Option<(Entity, u32)> = None;
        'outer: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if dx == 0 && dy == 0 { continue; }
                for &nb_entity in spatial.get(tx + dx, ty + dy) {
                    if nb_entity == *entity { continue; }
                    // Get neighbor's faction_id
                    if let Ok((_, nb_fm, _, _)) = query.get(nb_entity) {
                        found_neighbor = Some((nb_entity, nb_fm.faction_id));
                        break 'outer;
                    }
                }
            }
        }

        let Some((nb_entity, nb_faction)) = found_neighbor else { continue };

        // Use get_many_mut to safely borrow both entities at once
        let Ok([(_, mut fm, personality, transform), (_, mut nb_fm, _, _)]) =
            query.get_many_mut([*entity, nb_entity])
        else { continue };

        // Reset bond timer if target changed
        if fm.bond_target != Some(nb_entity) {
            fm.bond_target = Some(nb_entity);
            fm.bond_timer = 0;
        }

        let threshold = if *personality == Personality::Socialite {
            BOND_THRESHOLD.saturating_sub(60)
        } else {
            BOND_THRESHOLD
        };

        fm.bond_timer = fm.bond_timer.saturating_add(1);

        if fm.bond_timer >= threshold {
            fm.bond_timer = 0;
            fm.bond_target = None;

            let pos = transform.translation.truncate();
            let home_tx = (pos.x / TILE_SIZE).floor() as i16;
            let home_ty = (pos.y / TILE_SIZE).floor() as i16;

            let faction_id = if nb_faction == SOLO {
                let new_id = registry.create_faction((home_tx, home_ty));
                nb_fm.faction_id = new_id;
                nb_fm.bond_timer = 0;
                nb_fm.bond_target = None;
                registry.add_member(new_id); // for the neighbor
                new_id
            } else {
                nb_faction
            };

            fm.faction_id = faction_id;
            registry.add_member(faction_id);

            // Bonding builds affinity between the two agents
            if let Ok([rel1, rel2]) = rel_query.get_many_mut([*entity, nb_entity]) {
                if let Some(mut r) = rel1 {
                    r.update(nb_entity, 30);
                }
                if let Some(mut r) = rel2 {
                    r.update(*entity, 30);
                }
            }
        }
    }
}

// ── Social fill system ────────────────────────────────────────────────────────

pub fn social_fill_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    mut query: Query<(Entity, &mut Needs, &FactionMember, &Transform, &BucketSlot, &LodLevel)>,
) {
    query.par_iter_mut().for_each(|(entity, mut needs, member, transform, slot, lod)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }
        if member.faction_id == SOLO {
            return;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut nearby = 0u8;
        for dy in -SOCIAL_RADIUS..=SOCIAL_RADIUS {
            for dx in -SOCIAL_RADIUS..=SOCIAL_RADIUS {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other != entity {
                        nearby = nearby.saturating_add(1);
                    }
                }
            }
        }

        if nearby > 0 {
            needs.social = (needs.social - (nearby.min(10) * 3) as f32).max(0.0);
        }
    });
}

// ── Faction camp food system ──────────────────────────────────────────────────

pub fn faction_camp_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(
        &PersonAI,
        &mut EconomicAgent,
        &mut Needs,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (ai, mut agent, mut needs, member, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        let Some(faction) = registry.factions.get(&member.faction_id) else { continue };
        let home = faction.home_tile;

        // Only act when at the camp tile
        if ai.state != AiState::Working || ai.target_tile != home {
            continue;
        }

        let faction_id = member.faction_id;

        // Deposit surplus food
        let food_qty = agent.quantity_of(Good::Food);
        if food_qty > CAMP_KEEP {
            let deposit = food_qty - CAMP_KEEP;
            agent.remove_good(Good::Food, deposit);
            if let Some(f) = registry.factions.get_mut(&faction_id) {
                f.food_stock += deposit as f32;
            }
        }

        // Withdraw food if hungry and no personal food
        if needs.hunger > 100.0 && agent.quantity_of(Good::Food) == 0 {
            if let Some(f) = registry.factions.get_mut(&faction_id) {
                if f.food_stock >= 1.0 {
                    f.food_stock -= 1.0;
                    agent.add_good(Good::Food, 1);
                }
            }
        }
    }
}

// ── Helpers for job dispatch ──────────────────────────────────────────────────

impl FactionRegistry {
    pub fn home_tile(&self, faction_id: u32) -> Option<(i16, i16)> {
        self.factions.get(&faction_id).map(|f| f.home_tile)
    }

    pub fn food_stock(&self, faction_id: u32) -> f32 {
        self.factions.get(&faction_id).map(|f| f.food_stock).unwrap_or(0.0)
    }

    pub fn raid_target(&self, faction_id: u32) -> Option<u32> {
        self.factions.get(&faction_id).and_then(|f| f.raid_target)
    }

    pub fn is_under_raid(&self, faction_id: u32) -> bool {
        self.factions.get(&faction_id).map(|f| f.under_raid).unwrap_or(false)
    }
}
