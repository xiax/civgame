use super::goals::Personality;
use super::items::{spawn_or_merge_ground_item, GroundItem};
use super::jobs::{
    record_progress, record_progress_filtered, JobBoard, JobClaim, JobCompletedEvent, JobKind,
};
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::{AiState, PersonAI, Profession};
use super::schedule::{BucketSlot, SimClock};
use super::tasks::TaskKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::simulation::technology::{
    tech_def, ActivityKind, Era, TechId, ACTIVITY_COUNT, CROP_CULTIVATION, TECH_COUNT, TECH_TREE,
};
use crate::world::chunk::ChunkMap;
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
}

#[derive(Component)]
pub struct FactionCenter;

/// Marks the designated tribal chief of a faction.
/// Inserted on the faction founder at formation; re-elected by `chief_selection_system`
/// if the current chief leaves or dies.
#[derive(Component)]
pub struct FactionChief;

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
            .min_by_key(|&&(tx, ty)| (tx as i32 - from.0).abs() + (ty as i32 - from.1).abs())
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
        }
    }
}

// ── Faction culture & lineage ─────────────────────────────────────────────────

/// Architectural / strategic style a faction grows into. Picked at faction
/// creation; drives the settlement planner's zone-placement strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutStyle {
    /// Tight, overlapping zones around a small core.
    Compact,
    /// Wide gaps between zones; large outward footprint.
    Sprawling,
    /// Single E-W axis with branches.
    Linear,
    /// Concentric rings around the center (closest to current Neolithic).
    Radial,
    /// Inner residential core walled tightly; agriculture outside.
    Citadel,
}

impl LayoutStyle {
    pub const ALL: [LayoutStyle; 5] = [
        LayoutStyle::Compact,
        LayoutStyle::Sprawling,
        LayoutStyle::Linear,
        LayoutStyle::Radial,
        LayoutStyle::Citadel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayoutStyle::Compact => "Compact",
            LayoutStyle::Sprawling => "Sprawling",
            LayoutStyle::Linear => "Linear",
            LayoutStyle::Radial => "Radial",
            LayoutStyle::Citadel => "Citadel",
        }
    }
}

/// Per-faction architectural and behavioural personality. Rolled once at
/// faction creation from a deterministic seed. Drives the settlement planner,
/// build-candidate scoring, raid frequency, and ritual cadence.
#[derive(Clone, Debug)]
pub struct FactionCulture {
    pub style: LayoutStyle,
    /// 0..=255 — low = wide spacing, high = packed footprints.
    pub density: u8,
    /// 0..=255 — biases wall priority and ring count.
    pub defensive: u8,
    /// 0..=255 — biases shrines, monuments, ritual cadence.
    pub ceremonial: u8,
    /// 0..=255 — biases markets and storage.
    pub mercantile: u8,
    /// 0..=255 — biases barracks, raid frequency.
    pub martial: u8,
    pub seed: u32,
}

impl FactionCulture {
    /// Roll a deterministic culture from a seed (typically `home_tile + faction_id`).
    pub fn roll(seed: u32) -> Self {
        // Cheap deterministic hash steps — splitmix-ish.
        let mut s = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0xDEAD_BEEF);
        let mut next = || {
            s ^= s >> 16;
            s = s.wrapping_mul(0x85EB_CA6B);
            s ^= s >> 13;
            s = s.wrapping_mul(0xC2B2_AE35);
            s ^= s >> 16;
            s
        };
        let style = LayoutStyle::ALL[(next() as usize) % LayoutStyle::ALL.len()];
        // Style template + per-trait jitter ±40.
        let (mut den, mut def, mut cer, mut mer, mut mar) = match style {
            LayoutStyle::Compact => (200u8, 140, 110, 110, 110),
            LayoutStyle::Sprawling => (60, 90, 110, 130, 90),
            LayoutStyle::Linear => (130, 110, 100, 160, 110),
            LayoutStyle::Radial => (140, 130, 130, 110, 110),
            LayoutStyle::Citadel => (180, 220, 100, 90, 170),
        };
        let jitter = |base: u8, raw: u32| -> u8 {
            let delta = (raw % 81) as i32 - 40; // -40..=+40
            (base as i32 + delta).clamp(0, 255) as u8
        };
        den = jitter(den, next());
        def = jitter(def, next());
        cer = jitter(cer, next());
        mer = jitter(mer, next());
        mar = jitter(mar, next());
        Self {
            style,
            density: den,
            defensive: def,
            ceremonial: cer,
            mercantile: mer,
            martial: mar,
            seed,
        }
    }
}

/// Dynastic lineage information for a faction. Successor chiefs inherit a
/// modulated culture (small drift per generation); child agent names are
/// derived from `root`.
#[derive(Clone, Debug, Default)]
pub struct FactionLineage {
    /// Naming root (e.g., "Aren") used to generate descendant names.
    pub root: String,
    /// Founder's full name. Stable across the faction's lifetime.
    pub founder: String,
    /// Number of chief successions since founding.
    pub generation: u32,
}

impl FactionLineage {
    pub fn from_seed(seed: u32) -> Self {
        const ROOTS: &[&str] = &[
            "Aren", "Bryn", "Cael", "Doran", "Elin", "Faro", "Garen", "Hela", "Irek", "Joran",
            "Kael", "Lyr", "Maren", "Nyx", "Oran", "Pyra", "Quinn", "Rhea", "Sora", "Talin", "Uma",
            "Vale", "Wren", "Yara",
        ];
        const SUFFIX: &[&str] = &["", "-tha", "-mir", "-ros", "-vyn", "-dor", "-an", "-eth"];
        let r = ROOTS[(seed as usize) % ROOTS.len()];
        let s = SUFFIX[((seed >> 8) as usize) % SUFFIX.len()];
        Self {
            root: r.to_string(),
            founder: format!("{r}{s}"),
            generation: 0,
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
    /// The current tribal chief of this faction, if one has been designated.
    pub chief_entity: Option<Entity>,
    /// Architectural / behavioural personality. Drives planner, selector,
    /// raids, rituals.
    pub culture: FactionCulture,
    /// Dynastic naming + generation count. Updated at chief succession.
    pub lineage: FactionLineage,
    /// Tile of the structure currently being torn down for upgrade-replacement.
    /// `Some` while a deconstruct→rebuild cycle is in flight (one per faction).
    pub active_upgrade: Option<(i16, i16)>,
    /// Per-tick allocation across job kinds (gather/farm/build/craft/free).
    /// Recomputed every chief tick from the same pressure model that drives
    /// `compute_priority`. Consumed by `job_claim_system` as the per-kind cap
    /// instead of the old flat 50%-of-population rule.
    pub workforce_budget: crate::simulation::projects::WorkforceBudget,
    /// EMA per Good of how long material gather has been stagnating for this
    /// faction. Stage 3 reads this in `generate_candidates` to avoid picking
    /// blueprints that need a chronically-deficient input. Range 0..=255.
    pub material_deficit_ema: ahash::AHashMap<crate::economy::goods::Good, u8>,
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
        for def in TECH_TREE.iter() {
            if matches!(def.era, Era::Paleolithic) {
                techs.unlock(def.id);
            }
        }
        // Deterministic per-faction seed: home tile + faction id, packed.
        let seed = ((home_tile.0 as i32 as u32) << 16)
            ^ (home_tile.1 as i32 as u32)
            ^ id.wrapping_mul(0x9E37_79B9);
        let culture = FactionCulture::roll(seed);
        let lineage = FactionLineage::from_seed(seed);
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
                chief_entity: None,
                culture,
                lineage,
                active_upgrade: None,
                workforce_budget: crate::simulation::projects::WorkforceBudget::default(),
                material_deficit_ema: ahash::AHashMap::default(),
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
                // The initiating agent (outer-loop entity) becomes the founding chief.
                if let Some(fd) = registry.factions.get_mut(&new_id) {
                    fd.chief_entity = Some(*entity);
                }
                commands.entity(*entity).insert(FactionChief);
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
    mut board: ResMut<JobBoard>,
    mut job_completed: EventWriter<JobCompletedEvent>,
    mut ground_items: Query<&mut GroundItem>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &FactionMember,
        &Profession,
        &LodLevel,
        Option<&JobClaim>,
    )>,
) {
    for (worker, mut ai, mut agent, mut carrier, member, profession, lod, claim_opt) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Working || ai.task_id != TaskKind::DepositResource as u16 {
            continue;
        }

        let deposit_tx = ai.dest_tile.0 as i32;
        let deposit_ty = ai.dest_tile.1 as i32;

        // First: dump everything in hands. Hauling loads (Wood, Stone, Iron, ...) are
        // exactly what storage wants; food/tools that ended up in hands also go here.
        let mut hand_wood: u32 = 0;
        let mut hand_stone: u32 = 0;
        for stack in carrier.drop_all() {
            match stack.item.good {
                Good::Wood => hand_wood = hand_wood.saturating_add(stack.qty),
                Good::Stone => hand_stone = hand_stone.saturating_add(stack.qty),
                _ => {}
            }
            spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                deposit_tx,
                deposit_ty,
                stack.item.good,
                stack.qty,
            );
        }
        // Credit any Material Gather posting this worker holds for the
        // dropped wood/stone.
        if let Some(claim) = claim_opt {
            if hand_wood > 0 {
                record_progress_filtered(
                    &mut commands,
                    &mut board,
                    &mut job_completed,
                    claim,
                    JobKind::Gather,
                    Some(Good::Wood),
                    hand_wood,
                );
            }
            if hand_stone > 0 {
                record_progress_filtered(
                    &mut commands,
                    &mut board,
                    &mut job_completed,
                    claim,
                    JobKind::Gather,
                    Some(Good::Stone),
                    hand_stone,
                );
            }
        }

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
            // Sum calories of food deposited at faction storage so a Gather
            // job posting (if this worker holds one) can be credited.
            let mut deposited_calories: u32 = 0;
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
                deposited_calories =
                    deposited_calories.saturating_add(qty * good.nutrition() as u32);
            }
            if deposited_calories > 0 {
                if let Some(claim) = claim_opt {
                    record_progress(
                        &mut commands,
                        &mut board,
                        &mut job_completed,
                        claim,
                        JobKind::Gather,
                        deposited_calories,
                    );
                }
            }
        }
        let _ = worker; // silence unused if no further use

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

        // Deposit all crafted goods (Tools, Weapon, Armor, Shield, Cloth, Luxury).
        let mut crafted_drops: Vec<(Good, u32)> = Vec::new();
        for (it, q) in agent.inventory.iter_mut() {
            if *q > 0
                && matches!(
                    it.good,
                    Good::Tools
                        | Good::Weapon
                        | Good::Armor
                        | Good::Shield
                        | Good::Cloth
                        | Good::Luxury
                )
            {
                crafted_drops.push((it.good, *q));
                *q = 0;
            }
        }
        for (good, qty) in crafted_drops {
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

        // Deposit recovered construction materials (Wood from deconstruction).
        let wood_qty = agent.quantity_of(Good::Wood);
        if wood_qty > 0 {
            agent.remove_good(Good::Wood, wood_qty);
            spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                deposit_tx,
                deposit_ty,
                Good::Wood,
                wood_qty,
            );
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
    }
}

// ── Storage tile map maintenance ──────────────────────────────────────────────

pub fn update_storage_tile_map_system(
    mut map: ResMut<StorageTileMap>,
    mut hotspots: ResMut<HotspotFlowFields>,
    chunk_map: Res<ChunkMap>,
    changed_q: Query<(), Or<(Added<FactionStorageTile>, Changed<Transform>)>>,
    removed: RemovedComponents<FactionStorageTile>,
    all_q: Query<(&FactionStorageTile, &Transform)>,
) {
    if changed_q.is_empty() && removed.is_empty() {
        return;
    }
    // Snapshot the previous tile set so we can diff hotspot registrations.
    let prev: ahash::AHashSet<(i16, i16)> = map.tiles.keys().copied().collect();

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

    // Diff: register newly-added storage tiles, unregister removed ones.
    for &(tx, ty) in map.tiles.keys() {
        if !prev.contains(&(tx, ty)) {
            let z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
            hotspots.register((tx, ty, z), HotspotKind::Storage);
        }
    }
    for (tx, ty) in prev {
        if !map.tiles.contains_key(&(tx, ty)) {
            // We don't know the original Z; unregister at every plausible Z
            // by brute-force unregister at surface_z (the only Z storage
            // tiles get registered with above).
            let z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
            hotspots.unregister((tx, ty, z), HotspotKind::Storage);
        }
    }
}

// ── Faction-center hotspot sync ───────────────────────────────────────────────

/// Maintains `HotspotFlowFields` registrations for `FactionCenter` entities.
/// Each tribe's center is a high-traffic destination — caching a flow field
/// for it lets the worker skip per-agent A* on the final leg of a route.
pub fn sync_faction_center_hotspots_system(
    mut hotspots: ResMut<HotspotFlowFields>,
    chunk_map: Res<ChunkMap>,
    mut last_seen: Local<ahash::AHashMap<Entity, (i16, i16, i8)>>,
    mut removed: RemovedComponents<FactionCenter>,
    centers: Query<(Entity, &Transform), With<FactionCenter>>,
) {
    let mut current: ahash::AHashMap<Entity, (i16, i16, i8)> = ahash::AHashMap::new();
    for (entity, transform) in centers.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i16;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i16;
        let z = chunk_map.surface_z_at(tx as i32, ty as i32) as i8;
        current.insert(entity, (tx, ty, z));
    }

    // Unregister centers that were destroyed entirely.
    for entity in removed.read() {
        if let Some(prev) = last_seen.remove(&entity) {
            hotspots.unregister(prev, HotspotKind::FactionCenter);
        }
    }

    // Register newly-spawned centers; re-register if a center moved.
    for (entity, &tile) in current.iter() {
        match last_seen.get(entity) {
            Some(&prev) if prev == tile => {}
            Some(&prev) => {
                hotspots.unregister(prev, HotspotKind::FactionCenter);
                hotspots.register(tile, HotspotKind::FactionCenter);
            }
            None => {
                hotspots.register(tile, HotspotKind::FactionCenter);
            }
        }
    }

    *last_seen = current;
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
    // Materials from Blueprints — sum unmet need per ingredient across all deposit slots.
    for &bp_entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(bp_entity) {
            if let Some(faction) = registry.factions.get_mut(&bp.faction_id) {
                for i in 0..bp.deposit_count as usize {
                    let need = bp.deposits[i];
                    let unmet = need.needed.saturating_sub(need.deposited) as u32;
                    if unmet > 0 {
                        *faction.resource_demand.entry(need.good).or_insert(0) += unmet;
                    }
                }
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

// ── Chief selection system ────────────────────────────────────────────────────

/// Ensures every non-SOLO faction has a designated tribal chief.
/// Runs every 60 ticks. If the current chief has left or died, elects any
/// surviving faction member as the new chief.
pub fn chief_selection_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    member_query: Query<(Entity, &FactionMember)>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    // Build faction_id → member entities map from the current world state.
    let mut faction_members: AHashMap<u32, Vec<Entity>> = AHashMap::new();
    for (entity, member) in member_query.iter() {
        if member.faction_id != SOLO {
            faction_members
                .entry(member.faction_id)
                .or_default()
                .push(entity);
        }
    }

    for (&faction_id, faction) in registry.factions.iter_mut() {
        let members = match faction_members.get(&faction_id) {
            Some(m) if !m.is_empty() => m,
            _ => {
                faction.chief_entity = None;
                continue;
            }
        };

        let chief_valid = faction
            .chief_entity
            .map(|e| members.contains(&e))
            .unwrap_or(false);

        if !chief_valid {
            let old_chief = faction.chief_entity;
            let new_chief = members[0];
            faction.chief_entity = Some(new_chief);
            commands.entity(new_chief).insert(FactionChief);
            if let Some(old) = old_chief {
                if old != new_chief {
                    if let Some(mut ec) = commands.get_entity(old) {
                        ec.remove::<FactionChief>();
                    }
                }
                // Succession drift — only counts as a transition if there was
                // a prior chief (the founding chief sets generation 0).
                faction.lineage.generation = faction.lineage.generation.saturating_add(1);
                drift_culture(&mut faction.culture, faction.lineage.generation);
            }
        }
    }
}

/// Drift the five culture traits by ±10 deterministically based on the
/// generation count. Successive chiefs gradually shift settlement personality
/// without erasing the founder's identity. Layout style is left untouched —
/// architectural identity persists across generations.
fn drift_culture(culture: &mut FactionCulture, generation: u32) {
    let mut s = culture
        .seed
        .wrapping_add(generation.wrapping_mul(0x9E37_79B9));
    let mut next = || {
        s ^= s >> 16;
        s = s.wrapping_mul(0x85EB_CA6B);
        s ^= s >> 13;
        s
    };
    let drift = |val: u8, raw: u32| -> u8 {
        let delta = (raw % 21) as i32 - 10; // -10..=+10
        (val as i32 + delta).clamp(0, 255) as u8
    };
    culture.density = drift(culture.density, next());
    culture.defensive = drift(culture.defensive, next());
    culture.ceremonial = drift(culture.ceremonial, next());
    culture.mercantile = drift(culture.mercantile, next());
    culture.martial = drift(culture.martial, next());
}
