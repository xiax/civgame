use ahash::AHashMap;
use bevy::prelude::*;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{TILE_SIZE, tile_to_world};
use super::goals::Personality;
use super::memory::RelationshipMemory;
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI, Profession};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::technology::{
    TechId, ActivityKind, TECH_COUNT, ACTIVITY_COUNT, FIRE_MAKING, CROP_CULTIVATION, tech_def,
};

pub const SOLO: u32 = 0;
pub const BOND_THRESHOLD: u8 = 180;
const CAMP_KEEP: u8 = 0;
const SOCIAL_RADIUS: i32 = 3;

pub fn faction_profession_system(
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(&mut Profession, &FactionMember)>,
) {
    for (&faction_id, faction) in registry.factions.iter_mut() {
        if !faction.techs.has(CROP_CULTIVATION) {
            continue;
        }

        // Target: 1 farmer per 5 members if food is low.
        // Let's keep it simple for now: 20% of the population as farmers if food stock is below 100.
        let target_farmers = if faction.food_stock < 100.0 {
            (faction.member_count / 5).max(1)
        } else {
            0
        };

        let mut current_farmers = 0;

        for (prof, member) in query.iter() {
            if member.faction_id == faction_id {
                if *prof == Profession::Farmer {
                    current_farmers += 1;
                }
            }
        }

        if current_farmers < target_farmers {
            let to_assign = target_farmers - current_farmers;
            let mut assigned = 0;
            for (mut prof, member) in query.iter_mut() {
                if member.faction_id == faction_id && *prof == Profession::None {
                    *prof = Profession::Farmer;
                    assigned += 1;
                    if assigned >= to_assign {
                        break;
                    }
                }
            }
        } else if current_farmers > target_farmers {
            let to_unassign = current_farmers - target_farmers;
            let mut unassigned = 0;
            for (mut prof, member) in query.iter_mut() {
                if member.faction_id == faction_id && *prof == Profession::Farmer {
                    *prof = Profession::None;
                    unassigned += 1;
                    if unassigned >= to_unassign {
                        break;
                    }
                }
            }
        }
    }
}

#[derive(Component, Clone, Copy)]
pub struct FactionMember {
    pub faction_id:  u32,
    pub bond_target: Option<Entity>,
    pub bond_timer:  u8,
    /// Cooldown ticks after giving birth before reproduction need resets again.
    pub birth_cooldown: u32,
}

#[derive(Component)]
pub struct FactionCenter;

#[derive(Component)]
pub struct PlayerFactionMarker;

impl Default for FactionMember {
    fn default() -> Self {
        Self { faction_id: SOLO, bond_target: None, bond_timer: 0, birth_cooldown: 0 }
    }
}

/// u64 bitset storing which technologies are unlocked (bits 0-42).
#[derive(Clone, Debug, Default)]
pub struct FactionTechs(pub u64);

impl FactionTechs {
    #[inline]
    pub fn has(&self, id: TechId) -> bool { self.0 & (1u64 << id) != 0 }
    #[inline]
    pub fn unlock(&mut self, id: TechId) { self.0 |= 1u64 << id; }
}

/// Per-season activity counters, reset after each tech discovery pass.
#[derive(Clone, Debug, Default)]
pub struct ActivityLog(pub [u32; ACTIVITY_COUNT]);

impl ActivityLog {
    #[inline]
    pub fn increment(&mut self, kind: ActivityKind) {
        self.0[kind as usize] = self.0[kind as usize].saturating_add(1);
    }
    #[inline]
    pub fn get(&self, kind: ActivityKind) -> u32 { self.0[kind as usize] }
    pub fn reset(&mut self) { self.0 = [0; ACTIVITY_COUNT]; }
}

pub struct FactionData {
    pub food_stock:   f32,
    pub seed_stock:   u32,
    pub home_tile:    (i16, i16),
    pub member_count: u32,
    pub raid_target:  Option<u32>,
    pub under_raid:   bool,
    pub techs:        FactionTechs,
    pub activity_log: ActivityLog,
}

#[derive(Resource, Default)]
pub struct FactionRegistry {
    pub factions: AHashMap<u32, FactionData>,
    pub next_id:  u32,
}

#[derive(Resource, Default)]
pub struct PlayerFaction {
    pub faction_id: u32,
}

impl FactionRegistry {
    pub fn create_faction(&mut self, home_tile: (i16, i16)) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        let mut techs = FactionTechs::default();
        techs.unlock(FIRE_MAKING);
        self.factions.insert(id, FactionData {
            food_stock: 0.0,
            seed_stock: 0,
            home_tile, member_count: 0,
            raid_target: None, under_raid: false,
            techs,
            activity_log: ActivityLog::default(),
        });
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
    spatial:      Res<SpatialIndex>,
    clock:        Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(Entity, &mut Needs, &FactionMember, &Transform, &BucketSlot, &LodLevel)>,
) {
    for (entity, mut needs, member, transform, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
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
            if let Some(fd) = registry.factions.get_mut(&member.faction_id) {
                fd.activity_log.increment(ActivityKind::Socializing);
            }
        }
    }
}

// ── Faction camp food system ──────────────────────────────────────────────────

pub fn faction_camp_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Needs,
        &FactionMember,
        &Profession,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (ai, mut agent, needs, member, profession, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        let faction_id = member.faction_id;
        let (home, has_cultivation) = if let Some(faction) = registry.factions.get(&faction_id) {
            (faction.home_tile, faction.techs.has(CROP_CULTIVATION))
        } else {
            continue;
        };

        // Only act when at the camp tile
        if ai.state != AiState::Working || ai.target_tile != home {
            continue;
        }

        // Deposit surplus food
        let food_qty = agent.total_food();
        if food_qty > CAMP_KEEP {
            let mut deposit = food_qty - CAMP_KEEP;
            // Iterate and remove any edible goods
            for (it, q) in agent.inventory.iter_mut() {
                if it.good.is_edible() && *q > 0 {
                    let to_remove = (*q).min(deposit);
                    *q -= to_remove;
                    deposit -= to_remove;
                    if let Some(f) = registry.factions.get_mut(&faction_id) {
                        f.food_stock += to_remove as f32;
                    }
                }
                if deposit == 0 { break; }
            }
        }

        // Deposit seeds if non-farmer and faction has crop cultivation
        if *profession != Profession::Farmer && has_cultivation {
            let mut seeds_to_deposit = 0;
            for (it, q) in agent.inventory.iter_mut() {
                if it.good == Good::Seed && *q > 0 {
                    seeds_to_deposit += *q;
                    *q = 0;
                }
            }
            if seeds_to_deposit > 0 {
                if let Some(f) = registry.factions.get_mut(&faction_id) {
                    f.seed_stock += seeds_to_deposit as u32;
                }
            }
        }

        // Withdraw food if hungry and no personal food
        if needs.hunger > 100.0 && agent.total_food() == 0 {
            if let Some(f) = registry.factions.get_mut(&faction_id) {
                if f.food_stock >= 1.0 {
                    f.food_stock -= 1.0;
                    agent.add_good(Good::Fruit, 1);
                }
            }
        }
    }
}

// ── Helpers for task dispatch ──────────────────────────────────────────────────

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

impl FactionData {
    pub fn food_yield_multiplier(&self) -> f32 {
        1.0 + (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .map(|id| tech_def(id).bonus.food_yield_bonus)
            .sum::<f32>()
    }

    pub fn wood_yield_multiplier(&self) -> f32 {
        1.0 + (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .map(|id| tech_def(id).bonus.wood_yield_bonus)
            .sum::<f32>()
    }

    pub fn stone_yield_multiplier(&self) -> f32 {
        1.0 + (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .map(|id| tech_def(id).bonus.stone_yield_bonus)
            .sum::<f32>()
    }

    pub fn combat_damage_bonus(&self) -> u8 {
        (0..TECH_COUNT as u16)
            .filter(|&id| self.techs.has(id))
            .fold(0u8, |acc, id| acc.saturating_add(tech_def(id).bonus.combat_damage_bonus))
    }
}

pub fn center_camera_on_player_faction(
    player_faction: Res<PlayerFaction>,
    registry:       Res<FactionRegistry>,
    mut camera:     Query<&mut Transform, With<Camera>>,
) {
    let Some(data) = registry.factions.get(&player_faction.faction_id) else { return };
    let (htx, hty) = data.home_tile;
    let world_pos = tile_to_world(htx as i32, hty as i32);
    for mut transform in camera.iter_mut() {
        transform.translation.x = world_pos.x;
        transform.translation.y = world_pos.y;
    }
}
