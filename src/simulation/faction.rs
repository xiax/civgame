use super::goals::Personality;
use super::items::{spawn_or_merge_ground_item, GroundItem};
use super::jobs::{
    record_progress, record_progress_filtered, JobBoard, JobClaim, JobCompletedEvent, JobKind,
};
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::{AiState, PersonAI, Profession};
use super::plan::{ActivePlan, PlanHistory, PlanOutcome};
use super::plants::PlantKind;
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillKind, Skills};
use super::tasks::TaskKind;
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::simulation::technology::{
    tech_def, ActivityKind, Era, TechId, ACTIVITY_COUNT, CROP_CULTIVATION, HUNTING_SPEAR,
    TECH_COUNT, TECH_TREE,
};
use crate::world::chunk::ChunkMap;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use ahash::AHashMap;
use bevy::prelude::*;

pub const SOLO: u32 = 0;
pub const BOND_THRESHOLD: u8 = 180;
const CAMP_KEEP: u32 = 0;
const SOCIAL_RADIUS: i32 = 3;

// ── Chief-assigned hunting ───────────────────────────────────────────────────

/// Floor proportion of adults assigned as `Profession::Hunter` whenever the
/// faction has unlocked `HUNTING_SPEAR`. Scaled up by martial culture and
/// local prey density (see `faction_hunter_assignment_system`).
pub const HUNTER_MIN_RATIO: f32 = 0.20;

/// Tiles around `home_tile` the chief considers when picking a target species.
pub const HUNT_SCAN_RADIUS: i32 = 40;

/// Maximum age of a `HuntOrder` in ticks before the chief abandons a stalled
/// muster and waiters fall through. `TICKS_PER_DAY / 4` ≈ 15 game-hours / 45 s
/// real-time at 20 Hz — enough for stragglers without holding through the next
/// chief decision cycle.
pub const HUNT_PARTY_TIMEOUT: u64 = (TICKS_PER_DAY / 4) as u64;

/// Cadence at which `chief_hunt_order_system` re-decides each faction's
/// hunting target. Anchored at one game-day per faction so hunting reads as a
/// daily expedition, not a per-second reflex. Factions stagger across the day
/// via `tick % TICKS_PER_DAY == faction_id_offset`.
pub const HUNT_DECISION_CADENCE: u64 = TICKS_PER_DAY as u64;

/// Cadence for the cheap mid-day invalidation sweep. Cleared orders re-decide
/// at the next `HUNT_DECISION_CADENCE` boundary; this only catches the case
/// where a party finished or the prey emptied between full decision cycles.
pub const HUNT_INVALIDATE_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Cadence at which `faction_hunter_assignment_system` reconciles profession
/// counts. ~Once per quarter game-day; re-rolling every tick churns plans.
pub const HUNTER_ASSIGNMENT_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// A chief-issued hunting directive. Lives on `FactionData::hunt_order` and is
/// either a concrete `Hunt` (with mustering bookkeeping) or a fallback
/// `Scout` order to find new game.
#[derive(Clone, Debug)]
pub enum HuntOrder {
    Hunt {
        species: super::corpse::CorpseSpecies,
        area_tile: (i32, i32),
        target_party_size: u8,
        mustered: Vec<Entity>,
        deployed_tick: Option<u64>,
        posted_tick: u64,
    },
    Scout {
        posted_tick: u64,
    },
}

impl HuntOrder {
    pub fn posted_tick(&self) -> u64 {
        match self {
            HuntOrder::Hunt { posted_tick, .. } => *posted_tick,
            HuntOrder::Scout { posted_tick } => *posted_tick,
        }
    }
}

/// Chief-driven hunter assignment. Runs in Economy after
/// `compute_faction_storage_system` once per `HUNTER_ASSIGNMENT_CADENCE`. For
/// every faction:
///
/// - Compute a target headcount from `HUNTER_MIN_RATIO * adults`, scaled up
///   by `culture.martial` and local prey density. `HUNTING_SPEAR` is a hard
///   tech gate; without it, target = 0.
/// - Under target → promote the highest-Combat-skill `Profession::None` adult.
///   Skips Farmers (don't poach an established role).
/// - Over target → demote the lowest-Combat-skill `Hunter`, tear down their
///   `ActivePlan`, release any storage reservation, drop any carried corpse.
///
/// Density is read off `FactionData::nearby_prey_count`, which `chief_hunt_order_system`
/// refreshes alongside its decision cycle. We don't re-scan the spatial index
/// here — assignment runs more often than the chief and the density signal
/// only needs to be roughly current.
pub fn faction_hunter_assignment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    reservations: Res<StorageReservations>,
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut Profession,
        &FactionMember,
        &Skills,
        Option<&mut PersonAI>,
        Option<&mut PlanHistory>,
        Option<&crate::simulation::knowledge::PersonKnowledge>,
    )>,
) {
    if clock.tick % HUNTER_ASSIGNMENT_CADENCE != 0 {
        return;
    }

    // Snapshot per-faction target headcounts so we don't borrow registry
    // across the mutable query iteration.
    struct FactionTarget {
        adult_count: u32,
        hunter_target: usize,
    }
    let mut targets: AHashMap<u32, FactionTarget> = AHashMap::default();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        let has_tech = faction.techs.has(HUNTING_SPEAR);
        let adults = faction.member_count;
        let nearby = faction.nearby_prey_count as f32;
        let martial_scale = 0.5 + (faction.culture.martial as f32 / 255.0);
        let density_scale = if adults > 0 {
            (nearby / adults as f32).clamp(1.0, 2.0)
        } else {
            1.0
        };
        let mut target = if has_tech && adults > 0 {
            let floor = (adults as f32 * HUNTER_MIN_RATIO).round().max(1.0);
            (floor * martial_scale * density_scale).round() as usize
        } else {
            0
        };
        // Don't let hunters consume more than half the workforce.
        target = target.min((adults as usize) / 2);
        targets.insert(
            fid,
            FactionTarget {
                adult_count: adults,
                hunter_target: target,
            },
        );
    }

    // Per-faction snapshot of (entity, combat_skill) for current hunters and
    // None candidates. None candidates are pre-filtered to those who have
    // personally Learned HUNTING_SPEAR — the chief can post hunter slots
    // (faction-aware) but only members who actually know the technique are
    // promotable. Existing hunters are left alone; demotion will catch any
    // who lost the tech via LRU eviction.
    let mut by_faction_hunters: AHashMap<u32, Vec<(Entity, u32)>> = AHashMap::default();
    let mut by_faction_none: AHashMap<u32, Vec<(Entity, u32)>> = AHashMap::default();
    for (entity, prof, member, skills, _, _, knowledge_opt) in query.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        let combat = skills.0[SkillKind::Combat as usize];
        match *prof {
            Profession::Hunter => by_faction_hunters
                .entry(member.faction_id)
                .or_default()
                .push((entity, combat)),
            Profession::None => {
                let knows_hunting = knowledge_opt
                    .map(|k| k.has_learned(HUNTING_SPEAR))
                    .unwrap_or(false);
                if knows_hunting {
                    by_faction_none
                        .entry(member.faction_id)
                        .or_default()
                        .push((entity, combat));
                }
            }
            _ => {}
        }
    }

    let mut promote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut demote: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for (&fid, target) in &targets {
        let mut hunters = by_faction_hunters.remove(&fid).unwrap_or_default();
        let mut none = by_faction_none.remove(&fid).unwrap_or_default();
        let want = target.hunter_target;
        let _ = target.adult_count; // populated for inspector/logging
        if hunters.len() < want {
            none.sort_by(|a, b| b.1.cmp(&a.1));
            let need = want - hunters.len();
            for (e, _) in none.into_iter().take(need) {
                promote.insert(e);
            }
        } else if hunters.len() > want {
            hunters.sort_by(|a, b| a.1.cmp(&b.1));
            let extra = hunters.len() - want;
            for (e, _) in hunters.into_iter().take(extra) {
                demote.insert(e);
            }
        }
    }

    if promote.is_empty() && demote.is_empty() {
        return;
    }

    for (entity, mut prof, _member, _skills, ai_opt, history_opt, _knowledge) in
        query.iter_mut()
    {
        if promote.contains(&entity) {
            *prof = Profession::Hunter;
        } else if demote.contains(&entity) {
            *prof = Profession::None;
            if let Some(mut ai) = ai_opt {
                if ai.reserved_good.is_some() {
                    release_reservation(&reservations, &mut ai);
                }
                ai.carried_corpse = None;
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                ai.target_entity = None;
                ai.work_progress = 0;
            }
            if let Some(mut h) = history_opt {
                h.push(0, PlanOutcome::Aborted, clock.tick);
            }
            commands.entity(entity).remove::<ActivePlan>();
        }
    }
}

/// Per-faction chief decision: scan a `HUNT_SCAN_RADIUS` window around
/// `home_tile` for living Wolves/Deer, pick the species with highest count,
/// and post a `HuntOrder::Hunt` (or `HuntOrder::Scout` if nothing's nearby).
/// Runs once per `HUNT_DECISION_CADENCE` per faction; factions stagger
/// across the cadence by `faction_id` so the workload spreads. Also writes
/// `nearby_prey_count` to drive `faction_hunter_assignment_system`.
pub fn chief_hunt_order_system(
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    mut registry: ResMut<FactionRegistry>,
    prey_query: Query<
        (&Transform, &super::combat::Health),
        Or<(With<super::animals::Wolf>, With<super::animals::Deer>)>,
    >,
    wolf_q: Query<(), With<super::animals::Wolf>>,
    deer_q: Query<(), With<super::animals::Deer>>,
) {
    // Each faction's decision phase is `fid % HUNT_DECISION_CADENCE`, so
    // factions fire on different ticks throughout the day. Same for the
    // mid-day invalidation sweep, offset by half a cadence so it doesn't
    // collide with the decision tick. Modulo + branch is cheap; real work
    // only happens once per faction per cadence.
    let factions: Vec<u32> = registry.factions.keys().copied().collect();
    for fid in factions {
        if fid == SOLO {
            continue;
        }
        let phase_decide = (fid as u64) % HUNT_DECISION_CADENCE;
        let phase_invalidate =
            ((fid as u64).wrapping_add(HUNT_INVALIDATE_CADENCE / 2)) % HUNT_INVALIDATE_CADENCE;
        let do_decide = clock.tick % HUNT_DECISION_CADENCE == phase_decide;
        let do_invalidate = !do_decide
            && clock.tick % HUNT_INVALIDATE_CADENCE == phase_invalidate;
        if do_decide {
            decide_for_faction(
                fid,
                &mut registry,
                &spatial,
                &prey_query,
                &wolf_q,
                &deer_q,
                clock.tick,
            );
        } else if do_invalidate {
            invalidate_for_faction(fid, &mut registry, &spatial, &prey_query, &wolf_q, &deer_q, clock.tick);
        }
    }
}

fn invalidate_for_faction(
    fid: u32,
    registry: &mut FactionRegistry,
    spatial: &SpatialIndex,
    prey_query: &Query<
        (&Transform, &super::combat::Health),
        Or<(With<super::animals::Wolf>, With<super::animals::Deer>)>,
    >,
    wolf_q: &Query<(), With<super::animals::Wolf>>,
    deer_q: &Query<(), With<super::animals::Deer>>,
    tick: u64,
) {
    let Some(faction) = registry.factions.get_mut(&fid) else {
        return;
    };
    let Some(order) = faction.hunt_order.as_ref() else {
        return;
    };
    // Stale-muster timeout: the order has been live longer than agents
    // should patiently wait for stragglers, and the party never deployed.
    if let HuntOrder::Hunt { deployed_tick, .. } = order {
        if deployed_tick.is_none()
            && tick.saturating_sub(order.posted_tick()) > HUNT_PARTY_TIMEOUT
        {
            faction.hunt_order = None;
            return;
        }
    }
    // Target-area-empty: prey has moved on or been butchered.
    if let HuntOrder::Hunt { area_tile, species, .. } = order {
        let mut count = 0u32;
        let cx = area_tile.0 as i32;
        let cy = area_tile.1 as i32;
        for dy in -8..=8 {
            for dx in -8..=8 {
                let tx = cx + dx;
                let ty = cy + dy;
                for &e in spatial.get(tx, ty) {
                    let matches_species = match species {
                        super::corpse::CorpseSpecies::Wolf => wolf_q.get(e).is_ok(),
                        super::corpse::CorpseSpecies::Deer => deer_q.get(e).is_ok(),
                    };
                    if matches_species {
                        if let Ok((_, h)) = prey_query.get(e) {
                            if !h.is_dead() {
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
        if count == 0 {
            faction.hunt_order = None;
        }
    }
}

fn decide_for_faction(
    fid: u32,
    registry: &mut FactionRegistry,
    spatial: &SpatialIndex,
    prey_query: &Query<
        (&Transform, &super::combat::Health),
        Or<(With<super::animals::Wolf>, With<super::animals::Deer>)>,
    >,
    wolf_q: &Query<(), With<super::animals::Wolf>>,
    deer_q: &Query<(), With<super::animals::Deer>>,
    tick: u64,
) {
    let Some(faction) = registry.factions.get_mut(&fid) else {
        return;
    };
    if !faction.techs.has(HUNTING_SPEAR) {
        faction.hunt_order = None;
        faction.nearby_prey_count = 0;
        return;
    }
    let (htx, hty) = faction.home_tile;
    let mut wolf_count = 0u32;
    let mut deer_count = 0u32;
    let mut wolf_centroid = (0i64, 0i64);
    let mut deer_centroid = (0i64, 0i64);
    for dy in -HUNT_SCAN_RADIUS..=HUNT_SCAN_RADIUS {
        for dx in -HUNT_SCAN_RADIUS..=HUNT_SCAN_RADIUS {
            if dx * dx + dy * dy > HUNT_SCAN_RADIUS * HUNT_SCAN_RADIUS {
                continue;
            }
            let tx = htx as i32 + dx;
            let ty = hty as i32 + dy;
            for &e in spatial.get(tx, ty) {
                let Ok((_, health)) = prey_query.get(e) else {
                    continue;
                };
                if health.is_dead() {
                    continue;
                }
                if wolf_q.get(e).is_ok() {
                    wolf_count += 1;
                    wolf_centroid.0 += tx as i64;
                    wolf_centroid.1 += ty as i64;
                } else if deer_q.get(e).is_ok() {
                    deer_count += 1;
                    deer_centroid.0 += tx as i64;
                    deer_centroid.1 += ty as i64;
                }
            }
        }
    }
    faction.nearby_prey_count = wolf_count + deer_count;
    if wolf_count == 0 && deer_count == 0 {
        faction.hunt_order = Some(HuntOrder::Scout { posted_tick: tick });
        return;
    }
    let (species, count, centroid) = if wolf_count >= deer_count {
        (super::corpse::CorpseSpecies::Wolf, wolf_count, wolf_centroid)
    } else {
        (super::corpse::CorpseSpecies::Deer, deer_count, deer_centroid)
    };
    let area_tile = (
        (centroid.0 / count as i64) as i32,
        (centroid.1 / count as i64) as i32,
    );
    let target_party_size = match species {
        super::corpse::CorpseSpecies::Wolf => 4,
        super::corpse::CorpseSpecies::Deer => 2,
    };
    faction.hunt_order = Some(HuntOrder::Hunt {
        species,
        area_tile,
        target_party_size,
        mustered: Vec::new(),
        deployed_tick: None,
        posted_tick: tick,
    });
}

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
    pub tiles: AHashMap<(i32, i32), u32>,
    pub by_faction: AHashMap<u32, Vec<(i32, i32)>>,
}

impl StorageTileMap {
    pub fn nearest_for_faction(&self, faction_id: u32, from: (i32, i32)) -> Option<(i32, i32)> {
        self.by_faction
            .get(&faction_id)?
            .iter()
            .min_by_key(|&&(tx, ty)| (tx as i32 - from.0).abs() + (ty as i32 - from.1).abs())
            .copied()
    }
}

/// Tile-scoped reservations on storage stocks. Two agents committing to the
/// same one-unit stack used to be possible because the resolver only saw raw
/// `GroundItem.qty`; now the resolver subtracts entries here from the
/// effective stock. Each `WithdrawMaterial` dispatch increments the entry,
/// and every task-teardown path (success, race-loss, plan abort) decrements
/// it via `release_reservation` so the map stays consistent under churn.
///
/// Wrapped in a `Mutex` because `plan_execution_system` runs over agents in
/// parallel via `par_iter_mut`. Both reads (resolver) and writes (dispatch +
/// release) take the lock; critical sections are a single hashmap op each.
#[derive(Resource, Default)]
pub struct StorageReservations {
    inner: std::sync::Mutex<AHashMap<((i32, i32), Good), u32>>,
}

impl StorageReservations {
    pub fn add(&self, tile: (i32, i32), good: Good, qty: u32) {
        if qty == 0 {
            return;
        }
        let mut m = self.inner.lock().unwrap();
        *m.entry((tile, good)).or_insert(0) += qty;
    }

    pub fn sub(&self, tile: (i32, i32), good: Good, qty: u32) {
        if qty == 0 {
            return;
        }
        let mut m = self.inner.lock().unwrap();
        if let Some(slot) = m.get_mut(&(tile, good)) {
            *slot = slot.saturating_sub(qty);
            if *slot == 0 {
                m.remove(&(tile, good));
            }
        }
    }

    pub fn get(&self, tile: (i32, i32), good: Good) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .get(&(tile, good))
            .copied()
            .unwrap_or(0)
    }

    /// Snapshot total reserved qty across all (tile, good) pairs. Used by
    /// the inspector for debugging.
    pub fn total(&self) -> u32 {
        self.inner.lock().unwrap().values().sum()
    }
}

/// Decrement and clear the reservation tracked on a `PersonAI`. Safe to call
/// from any teardown path; no-ops when the agent has no live reservation.
pub fn release_reservation(
    reservations: &StorageReservations,
    ai: &mut crate::simulation::person::PersonAI,
) {
    if let Some(good) = ai.reserved_good {
        reservations.sub(ai.reserved_tile, good, ai.reserved_qty as u32);
    }
    ai.reserved_good = None;
    ai.reserved_qty = 0;
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
    pub fn grain_seed_total(&self) -> u32 {
        self.totals.get(&Good::GrainSeed).copied().unwrap_or(0)
    }
    pub fn berry_seed_total(&self) -> u32 {
        self.totals.get(&Good::BerrySeed).copied().unwrap_or(0)
    }
    pub fn seed_total(&self) -> u32 {
        self.grain_seed_total() + self.berry_seed_total()
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
    pub home_tile: (i32, i32),
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
    pub active_upgrade: Option<(i32, i32)>,
    /// Per-tick allocation across job kinds (gather/farm/build/craft/free).
    /// Recomputed every chief tick from the same pressure model that drives
    /// `compute_priority`. Consumed by `job_claim_system` as the per-kind cap
    /// instead of the old flat 50%-of-population rule.
    pub workforce_budget: crate::simulation::projects::WorkforceBudget,
    /// EMA per Good of how long material gather has been stagnating for this
    /// faction. Stage 3 reads this in `generate_candidates` to avoid picking
    /// blueprints that need a chronically-deficient input. Range 0..=255.
    pub material_deficit_ema: ahash::AHashMap<crate::economy::goods::Good, u8>,
    /// Anticipatory stockpile reserves: target storage levels per Good that
    /// the chief asks workers to maintain even before any blueprint demands
    /// them. Computed each chief tick from member count, culture traits,
    /// and tech foresight. Consumed by `chief_job_posting_system` to size
    /// Stockpile postings, and by `goal_update_system` to pick a fallback
    /// gather goal for unclaimed workers. Range 0..=u32::MAX.
    pub material_targets: ahash::AHashMap<crate::economy::goods::Good, u32>,
    /// Active hunting directive (`Hunt` or `Scout`) issued by the chief.
    /// Refreshed by `chief_hunt_order_system` once per game-day, with a
    /// mid-day invalidation sweep that clears spent / empty targets.
    pub hunt_order: Option<HuntOrder>,
    /// Count of living Wolf+Deer entities scanned within `HUNT_SCAN_RADIUS`
    /// of `home_tile` on the most recent chief decision. Drives
    /// `faction_hunter_assignment_system`'s density scaling so factions in
    /// game-rich areas grow more hunters above the 20% floor.
    pub nearby_prey_count: u32,
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
    pub fn create_faction(&mut self, home_tile: (i32, i32)) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        // FactionTechs starts empty; `sync_faction_techs_from_chief_system`
        // populates it from the chief's PersonKnowledge.aware bitset every
        // Economy tick. The founding chief already carries Paleolithic
        // awareness from `PersonKnowledge::paleolithic_seed` at spawn.
        let techs = FactionTechs::default();
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
                material_targets: ahash::AHashMap::default(),
                hunt_order: None,
                nearby_prey_count: 0,
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
            let home_tx = (pos.x / TILE_SIZE).floor() as i32;
            let home_ty = (pos.y / TILE_SIZE).floor() as i32;

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
    mut discovery_events: EventWriter<crate::simulation::knowledge::DiscoveryActionEvent>,
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
            discovery_events.send(crate::simulation::knowledge::DiscoveryActionEvent {
                actor: entity,
                activity: ActivityKind::Socializing,
            });
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
                    JobKind::Stockpile,
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
                    JobKind::Stockpile,
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
                        JobKind::Stockpile,
                        deposited_calories,
                    );
                }
            }
        }
        let _ = worker; // silence unused if no further use

        // Deposit any seeds the agent is carrying in inventory. Hand seeds
        // were already dumped via `carrier.drop_all()` above. Iterating
        // `PlantKind::ALL` keeps this loop in sync with the seed↔plant table
        // — adding a new seed only needs an arm in `PlantKind::seed_good()`.
        // Farmers deposit too: PlantFromStorage withdraws seeds back as
        // needed, which keeps the `SI_STORAGE_*_SEED` state slots meaningful.
        let has_cultivation = registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.techs.has(CROP_CULTIVATION))
            .unwrap_or(false);
        if has_cultivation {
            for seed_good in PlantKind::ALL.iter().filter_map(|k| k.seed_good()) {
                let mut qty: u32 = 0;
                for (it, q) in agent.inventory.iter_mut() {
                    if it.good == seed_good && *q > 0 {
                        qty += *q;
                        *q = 0;
                    }
                }
                if qty > 0 {
                    spawn_or_merge_ground_item(
                        &mut commands,
                        &spatial,
                        &mut ground_items,
                        deposit_tx,
                        deposit_ty,
                        seed_good,
                        qty,
                    );
                }
            }
        }

        // Deposit all crafted goods (Tools, Weapon, Armor, Shield, Cloth, Luxury).
        // Preserve the full `Item` (material + quality + stats) through storage
        // so an equipped Iron Spear keeps its damage_bonus after a withdraw.
        let mut crafted_drops: Vec<(crate::economy::item::Item, u32)> = Vec::new();
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
                crafted_drops.push((*it, *q));
                *q = 0;
            }
        }
        for (item, qty) in crafted_drops {
            crate::simulation::items::spawn_or_merge_ground_item_full(
                &mut commands,
                &spatial,
                &mut ground_items,
                deposit_tx,
                deposit_ty,
                item,
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
    let prev: ahash::AHashSet<(i32, i32)> = map.tiles.keys().copied().collect();

    map.tiles.clear();
    map.by_faction.clear();
    for (tile, transform) in all_q.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
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
    mut last_seen: Local<ahash::AHashMap<Entity, (i32, i32, i8)>>,
    mut removed: RemovedComponents<FactionCenter>,
    centers: Query<(Entity, &Transform), With<FactionCenter>>,
) {
    let mut current: ahash::AHashMap<Entity, (i32, i32, i8)> = ahash::AHashMap::new();
    for (entity, transform) in centers.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
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
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
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
    pub fn home_tile(&self, faction_id: u32) -> Option<(i32, i32)> {
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

        // Crafted-good demand: scales with member count. Drives
        // `chief_job_posting_system`'s recipe selection (highest output-good
        // deficit wins).
        faction.resource_demand.insert(Good::Tools, faction.member_count.div_ceil(2));
        faction.resource_demand.insert(Good::Weapon, faction.member_count.div_ceil(2));
        faction.resource_demand.insert(Good::Cloth, faction.member_count.div_ceil(2));
        faction.resource_demand.insert(Good::Luxury, faction.member_count.div_ceil(3));
        faction.resource_demand.insert(Good::Shield, faction.member_count.div_ceil(4));
        faction.resource_demand.insert(Good::Armor, faction.member_count.div_ceil(4));
    }
}

// ── Material stockpile target system ──────────────────────────────────────────

/// Refresh `FactionData::material_targets` for every faction. Targets are
/// anticipatory reserves the chief asks workers to keep in storage even when
/// no blueprint currently demands them. Driven by member count, culture
/// traits, and tech foresight; runs every 60 ticks in Economy.
pub fn update_material_targets_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
) {
    if clock.tick % 60 != 0 {
        return;
    }
    for faction in registry.factions.values_mut() {
        if faction.member_count == 0 {
            faction.material_targets.clear();
            continue;
        }
        // Baseline: scale with member count so larger tribes hold more
        // headroom for spontaneous upgrades.
        let members = faction.member_count;
        let mut wood_target = (members * 2).max(8);
        let mut stone_target = members.max(4);

        // Culture modulation. Trait values are 0..=255; clamp the multiplier
        // to a sane range so high-defense factions stockpile ~50% more stone.
        let scale_u32 = |base: u32, trait_value: u8, max_bonus: f32| -> u32 {
            let t = trait_value as f32 / 255.0;
            let mult = 1.0 + t * max_bonus;
            (base as f32 * mult).round() as u32
        };
        // Defensive cultures want more stone (walls); martial want both.
        stone_target = scale_u32(stone_target, faction.culture.defensive, 0.5);
        stone_target = scale_u32(stone_target, faction.culture.martial, 0.25);
        wood_target = scale_u32(wood_target, faction.culture.martial, 0.25);
        // Ceremonial bumps stone (shrines/monuments).
        stone_target = scale_u32(stone_target, faction.culture.ceremonial, 0.3);
        // Mercantile bumps wood (markets, granaries).
        wood_target = scale_u32(wood_target, faction.culture.mercantile, 0.25);

        // Tech foresight: once flint knapping or settlement is unlocked,
        // construction tier-ups will start consuming stone in volume.
        if faction.techs.has(crate::simulation::technology::FLINT_KNAPPING) {
            stone_target = stone_target.saturating_add(4);
        }
        if faction.techs.has(crate::simulation::technology::PERM_SETTLEMENT) {
            wood_target = wood_target.saturating_add(4);
        }

        faction.material_targets.insert(Good::Wood, wood_target);
        faction.material_targets.insert(Good::Stone, stone_target);
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

/// Sole writer of `FactionData.techs`. Each Economy tick, project the chief's
/// `PersonKnowledge.aware` bitset onto the faction so existing read sites
/// (plan filters, recipe gates, building gates, era checks) reflect the
/// leader's awareness. If the faction has no chief, leave the previous value
/// untouched — `chief_selection_system` runs every 60 ticks and will refill it.
pub fn sync_faction_techs_from_chief_system(
    mut registry: ResMut<FactionRegistry>,
    chief_q: Query<&crate::simulation::knowledge::PersonKnowledge>,
) {
    for (_id, faction) in registry.factions.iter_mut() {
        let Some(chief) = faction.chief_entity else {
            continue;
        };
        let Ok(knowledge) = chief_q.get(chief) else {
            continue;
        };
        // Mask to valid tech bits (lower TECH_COUNT) to keep the bitset clean.
        let mask = if TECH_COUNT >= 64 {
            u64::MAX
        } else {
            (1u64 << TECH_COUNT) - 1
        };
        faction.techs.0 = knowledge.aware & mask;
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
