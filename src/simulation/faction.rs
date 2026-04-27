use super::goals::Personality;
use super::items::{spawn_or_merge_ground_item, GroundItem};
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::{AiState, PersonAI, Profession};
use super::schedule::{BucketSlot, SimClock};
use super::tasks::TaskKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::technology::{
    tech_def, ActivityKind, TechId, ACTIVITY_COUNT, CROP_CULTIVATION, FIRE_MAKING, TECH_COUNT,
};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use ahash::AHashMap;
use bevy::prelude::*;

pub const SOLO: u32 = 0;
pub const BOND_THRESHOLD: u8 = 180;
const CAMP_KEEP: u32 = 0;
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
        let target_farmers = if faction.storage.food_total() < 100.0 {
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
    pub faction_id: u32,
    pub bond_target: Option<Entity>,
    pub bond_timer: u8,
    /// Cooldown ticks after giving birth before reproduction need resets again.
    pub birth_cooldown: u32,
}

#[derive(Component)]
pub struct FactionCenter;

#[derive(Component)]
pub struct PlayerFactionMarker;

/// Marks an entity as a storage drop-off point for a faction.
/// Spawned at the faction's home tile on creation; additional tiles can be added later.
#[derive(Component, Clone, Copy)]
pub struct FactionStorageTile {
    pub faction_id: u32,
}

/// Fast lookup from tile coords to faction_id for all storage tiles.
#[derive(Resource, Default)]
pub struct StorageTileMap {
    pub tiles: AHashMap<(i16, i16), u32>,
    pub by_faction: AHashMap<u32, Vec<(i16, i16)>>,
}

impl StorageTileMap {
    pub fn nearest_for_faction(&self, faction_id: u32, from: (i32, i32)) -> Option<(i16, i16)> {
        self.by_faction
            .get(&faction_id)?
            .iter()
            .min_by_key(|&&(tx, ty)| {
                (tx as i32 - from.0).abs() + (ty as i32 - from.1).abs()
            })
            .copied()
    }
}

/// Computed cache of goods stored on all storage tiles for a faction.
/// Updated each Economy tick by `compute_faction_storage_system`.
#[derive(Default, Clone)]
pub struct FactionStorage {
    pub totals: AHashMap<Good, u32>,
}

impl FactionStorage {
    pub fn food_total(&self) -> f32 {
        [Good::Fruit, Good::Meat, Good::Grain]
            .iter()
            .map(|g| self.totals.get(g).copied().unwrap_or(0) as f32)
            .sum()
    }
    pub fn seed_total(&self) -> u32 {
        self.totals.get(&Good::Seed).copied().unwrap_or(0)
    }
}

impl Default for FactionMember {
    fn default() -> Self {
        Self {
            faction_id: SOLO,
            bond_target: None,
            bond_timer: 0,
            birth_cooldown: 0,
        }
    }
}

/// u64 bitset storing which technologies are unlocked (bits 0-42).
#[derive(Clone, Debug, Default)]
pub struct FactionTechs(pub u64);

impl FactionTechs {
    #[inline]
    pub fn has(&self, id: TechId) -> bool {
        self.0 & (1u64 << id) != 0
    }
    #[inline]
    pub fn unlock(&mut self, id: TechId) {
        self.0 |= 1u64 << id;
    }
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
    pub fn get(&self, kind: ActivityKind) -> u32 {
        self.0[kind as usize]
    }
    pub fn reset(&mut self) {
        self.0 = [0; ACTIVITY_COUNT];
    }
}

pub struct FactionData {
    pub storage: FactionStorage,
    pub home_tile: (i16, i16),
    pub member_count: u32,
    pub raid_target: Option<u32>,
    pub under_raid: bool,
    pub techs: FactionTechs,
    pub activity_log: ActivityLog,
    pub resource_supply: ahash::AHashMap<crate::economy::goods::Good, u32>,
    pub resource_demand: ahash::AHashMap<crate::economy::goods::Good, u32>,
}

#[derive(Resource, Default)]
pub struct FactionRegistry {
    pub factions: AHashMap<u32, FactionData>,
    pub next_id: u32,
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
        self.factions.insert(
            id,
            FactionData {
                storage: FactionStorage::default(),
                home_tile,
                member_count: 0,
                raid_target: None,
                under_raid: false,
                techs,
                activity_log: ActivityLog::default(),
                resource_supply: ahash::AHashMap::default(),
                resource_demand: ahash::AHashMap::default(),
            },
        );
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
    mut commands: Commands,
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
                if dx == 0 && dy == 0 {
                    continue;
                }
                for &nb_entity in spatial.get(tx + dx, ty + dy) {
                    if nb_entity == *entity {
                        continue;
                    }
                    // Get neighbor's faction_id
                    if let Ok((_, nb_fm, _, _)) = query.get(nb_entity) {
                        found_neighbor = Some((nb_entity, nb_fm.faction_id));
                        break 'outer;
                    }
                }
            }
        }

        let Some((nb_entity, nb_faction)) = found_neighbor else {
            continue;
        };

        // Use get_many_mut to safely borrow both entities at once
        let Ok([(_, mut fm, personality, transform), (_, mut nb_fm, _, _)]) =
            query.get_many_mut([*entity, nb_entity])
        else {
            continue;
        };

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
                // Spawn a storage tile at the new faction's home position
                let world_pos = tile_to_world(home_tx as i32, home_ty as i32);
                commands.spawn((
                    FactionStorageTile { faction_id: new_id },
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
                    GlobalTransform::default(),
                    Visibility::Hidden,
                    InheritedVisibility::default(),
                ));
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
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(
        Entity,
        &mut Needs,
        &FactionMember,
        &Transform,
        &BucketSlot,
        &LodLevel,
    )>,
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

// ── Drop items at destination system ─────────────────────────────────────────

pub fn drop_items_at_destination_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    registry: Res<FactionRegistry>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &FactionMember,
        &Profession,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, member, profession, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::DepositResource as u16 {
            continue;
        }

        let deposit_tx = ai.dest_tile.0 as i32;
        let deposit_ty = ai.dest_tile.1 as i32;

        let food_qty = agent.total_food();
        if food_qty > CAMP_KEEP {
            let mut deposit = food_qty - CAMP_KEEP;
            let mut drops: Vec<(Good, u32)> = Vec::new();
            for (it, q) in agent.inventory.iter_mut() {
                if it.good.is_edible() && *q > 0 {
                    let to_remove = (*q).min(deposit);
                    *q -= to_remove;
                    deposit -= to_remove;
                    drops.push((it.good, to_remove));
                }
                if deposit == 0 {
                    break;
                }
            }
            for (good, qty) in drops {
                spawn_or_merge_ground_item(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    deposit_tx,
                    deposit_ty,
                    good,
                    qty,
                );
            }
        }

        // Non-farmers deposit seeds so farmers can plant them.
        let has_cultivation = registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.techs.has(CROP_CULTIVATION))
            .unwrap_or(false);
        if *profession != Profession::Farmer && has_cultivation {
            let mut seeds: u32 = 0;
            for (it, q) in agent.inventory.iter_mut() {
                if it.good == Good::Seed && *q > 0 {
                    seeds += *q;
                    *q = 0;
                }
            }
            if seeds > 0 {
                spawn_or_merge_ground_item(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    deposit_tx,
                    deposit_ty,
                    Good::Seed,
                    seeds,
                );
            }
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
    }
}

// ── Storage tile map maintenance ──────────────────────────────────────────────

pub fn update_storage_tile_map_system(
    mut map: ResMut<StorageTileMap>,
    changed_q: Query<
        (),
        Or<(Added<FactionStorageTile>, Changed<Transform>)>,
    >,
    removed: RemovedComponents<FactionStorageTile>,
    all_q: Query<(&FactionStorageTile, &Transform)>,
) {
    if changed_q.is_empty() && removed.is_empty() {
        return;
    }
    map.tiles.clear();
    map.by_faction.clear();
    for (tile, transform) in all_q.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i16;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i16;
        map.tiles.insert((tx, ty), tile.faction_id);
        map.by_faction
            .entry(tile.faction_id)
            .or_default()
            .push((tx, ty));
    }
}

// ── Faction storage totals computation ───────────────────────────────────────

pub fn compute_faction_storage_system(
    storage_tile_map: Res<StorageTileMap>,
    ground_items: Query<(&GroundItem, &Transform)>,
    mut registry: ResMut<FactionRegistry>,
) {
    for faction in registry.factions.values_mut() {
        faction.storage.totals.clear();
    }

    for (gi, transform) in ground_items.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i16;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i16;
        let Some(&faction_id) = storage_tile_map.tiles.get(&(tx, ty)) else {
            continue;
        };
        let Some(faction) = registry.factions.get_mut(&faction_id) else {
            continue;
        };
        *faction.storage.totals.entry(gi.item.good).or_insert(0) += gi.qty;
    }
}

// ── Helpers for task dispatch ──────────────────────────────────────────────────

impl FactionRegistry {
    pub fn home_tile(&self, faction_id: u32) -> Option<(i16, i16)> {
        self.factions.get(&faction_id).map(|f| f.home_tile)
    }

    pub fn food_stock(&self, faction_id: u32) -> f32 {
        self.factions
            .get(&faction_id)
            .map(|f| f.storage.food_total())
            .unwrap_or(0.0)
    }

    pub fn raid_target(&self, faction_id: u32) -> Option<u32> {
        self.factions.get(&faction_id).and_then(|f| f.raid_target)
    }

    pub fn is_under_raid(&self, faction_id: u32) -> bool {
        self.factions
            .get(&faction_id)
            .map(|f| f.under_raid)
            .unwrap_or(false)
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
            .fold(0u8, |acc, id| {
                acc.saturating_add(tech_def(id).bonus.combat_damage_bonus)
            })
    }
}

pub fn center_camera_on_player_faction(
    player_faction: Res<PlayerFaction>,
    registry: Res<FactionRegistry>,
    mut camera: Query<&mut Transform, With<Camera>>,
) {
    let Some(data) = registry.factions.get(&player_faction.faction_id) else {
        return;
    };
    let (htx, hty) = data.home_tile;
    let world_pos = tile_to_world(htx as i32, hty as i32);
    for mut transform in camera.iter_mut() {
        transform.translation.x = world_pos.x;
        transform.translation.y = world_pos.y;
    }
}

pub fn resource_demand_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    agent_query: Query<(&FactionMember, &EconomicAgent)>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    for faction in registry.factions.values_mut() {
        faction.resource_supply.clear();
        faction.resource_demand.clear();
    }

    // 1. Tally supply (agents' inventories + faction stocks)
    for (member, agent) in agent_query.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        if let Some(faction) = registry.factions.get_mut(&member.faction_id) {
            for (item, qty) in &agent.inventory {
                if *qty > 0 {
                    *faction.resource_supply.entry(item.good).or_insert(0) += *qty;
                }
            }
        }
    }

    for faction in registry.factions.values_mut() {
        for (&good, &qty) in &faction.storage.totals {
            *faction.resource_supply.entry(good).or_insert(0) += qty;
        }
    }

    // 2. Tally demand
    // Materials from Blueprints
    for &bp_entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(bp_entity) {
            if let Some(faction) = registry.factions.get_mut(&bp.faction_id) {
                let good = match bp.kind {
                    crate::simulation::construction::BuildSiteKind::Wall => Good::Wood,
                    crate::simulation::construction::BuildSiteKind::Bed => Good::Wood,
                };
                let needed = bp.wood_needed.saturating_sub(bp.wood_deposited) as u32;
                *faction.resource_demand.entry(good).or_insert(0) += needed;
            }
        }
    }

    // Food demand from population size
    for faction in registry.factions.values_mut() {
        let food_demand = faction.member_count * 10;
        faction.resource_demand.insert(Good::Fruit, food_demand);
        faction.resource_demand.insert(Good::Meat, food_demand);
        faction.resource_demand.insert(Good::Grain, food_demand);
    }
}
