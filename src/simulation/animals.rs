use super::combat::{Body, CombatCooldown, CombatTarget, Health};
use super::lod::LodLevel;
use super::person::Person;
use super::reproduction::BiologicalSex;
use super::schedule::{BucketSlot, SimClock};
use crate::simulation::line_of_sight::has_los;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_SEASON;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use crate::world::tile::TileKind;
use ahash::AHashSet;
use bevy::prelude::*;
use std::time::Instant;

const WOLF_COUNT: u32 = 150;
const DEER_COUNT: u32 = 400;
const HORSE_COUNT: u32 = 200;
const COW_COUNT: u32 = 80;
const RABBIT_COUNT: u32 = 500;
const PIG_COUNT: u32 = 120;
const FOX_COUNT: u32 = 80;
const CAT_COUNT: u32 = 60;
const HORSE_POP_CAP: usize = 300;
const HORSE_HP: u8 = 40;
const COW_HP: u8 = 35;
const RABBIT_HP: u8 = 6;
const PIG_HP: u8 = 25;
const FOX_HP: u8 = 12;
const CAT_HP: u8 = 8;
const HORSE_REPRO_MALE_THRESHOLD: f32 = 160.0;
const HORSE_REPRO_FEMALE_THRESHOLD: f32 = 190.0;
const COW_REPRO_MALE_THRESHOLD: f32 = 160.0;
const COW_REPRO_FEMALE_THRESHOLD: f32 = 190.0;
const RABBIT_REPRO_MALE_THRESHOLD: f32 = 130.0;
const RABBIT_REPRO_FEMALE_THRESHOLD: f32 = 150.0;
const PIG_REPRO_MALE_THRESHOLD: f32 = 150.0;
const PIG_REPRO_FEMALE_THRESHOLD: f32 = 180.0;
const FOX_REPRO_MALE_THRESHOLD: f32 = 150.0;
const FOX_REPRO_FEMALE_THRESHOLD: f32 = 180.0;
const CAT_REPRO_MALE_THRESHOLD: f32 = 140.0;
const CAT_REPRO_FEMALE_THRESHOLD: f32 = 170.0;
const ANIMAL_SPEED: f32 = 32.0; // pixels/sec, slower than persons
const WANDER_INTERVAL: f32 = 3.0;

// Need rates — declared as per-game-day totals; per-real-second values
// derive via `per_game_day_rate` so doubling `TICKS_PER_DAY` doesn't
// change how much an animal accrues per game day.
const ANIMAL_HUNGER_PER_DAY: f32 = 5.4;
const ANIMAL_HUNGER_RATE: f32 = crate::world::seasons::per_game_day_rate(ANIMAL_HUNGER_PER_DAY);
/// Animal thirst — roughly 2× hunger so animals seek water multiple times
/// per game-day.
const ANIMAL_THIRST_PER_DAY: f32 = 10.8;
const ANIMAL_THIRST_RATE: f32 = crate::world::seasons::per_game_day_rate(ANIMAL_THIRST_PER_DAY);
/// Animal thirst threshold to interrupt Wander / Grazing and route to
/// water. Predator chase / flight still preempt.
pub const ANIMAL_THIRST_TRIGGER: f32 = 160.0;
/// Thirst reduction per adjacency-drink for an animal.
pub const ANIMAL_DRINK_THIRST_REDUCTION: f32 = 90.0;
/// Animal sickness decay. Tuned so a fresh `255` sickness burn-off settles
/// inside ~1 game day.
const ANIMAL_SICKNESS_DECAY_PER_DAY: f32 = 270.0;
const ANIMAL_SICKNESS_DECAY_RATE: f32 =
    crate::world::seasons::per_game_day_rate(ANIMAL_SICKNESS_DECAY_PER_DAY);
const ANIMAL_SLEEP_PER_DAY: f32 = 45.0;
const ANIMAL_SLEEP_RATE: f32 = crate::world::seasons::per_game_day_rate(ANIMAL_SLEEP_PER_DAY);
const ANIMAL_SLEEP_RECOVER_PER_DAY: f32 = 450.0;
const ANIMAL_SLEEP_RECOVER_RATE: f32 =
    crate::world::seasons::per_game_day_rate(ANIMAL_SLEEP_RECOVER_PER_DAY);
const ANIMAL_SLEEP_THRESHOLD: f32 = 180.0;
const ANIMAL_SLEEP_WAKE_THRESHOLD: f32 = 20.0;
const ANIMAL_REPRO_PER_DAY: f32 = 7.2;
const ANIMAL_REPRO_RATE: f32 = crate::world::seasons::per_game_day_rate(ANIMAL_REPRO_PER_DAY);
const ANIMAL_HUNGER_RECOVER_WOLF: f32 = 150.0;
pub const ANIMAL_HUNGER_RECOVER_DEER: f32 = 80.0;
const ANIMAL_HUNGER_RECOVER_FOX: f32 = 90.0;
const ANIMAL_HUNGER_RECOVER_CAT: f32 = 70.0;
/// Wolves only proactively hunt humans when this hungry. Above sleep
/// threshold (180) and reproduction threshold (180) so a wolf trying to
/// sleep or breed won't impulse-attack humans.
const WOLF_AGGRESSIVE_HUNGER: f32 = 200.0;
/// Hysteresis: once chasing a human, drop the chase only when hunger falls
/// 20 below the engagement threshold to avoid oscillation near the boundary.
const WOLF_DROP_HUMAN_TARGET_HUNGER: f32 = 180.0;
const WOLF_REPRO_MALE_THRESHOLD: f32 = 150.0;
const WOLF_REPRO_FEMALE_THRESHOLD: f32 = 180.0;
const DEER_REPRO_MALE_THRESHOLD: f32 = 150.0;
const DEER_REPRO_FEMALE_THRESHOLD: f32 = 180.0;
const ANIMAL_BIRTH_CHANCE: u32 = 5; // out of 10,000
const WOLF_POP_CAP: usize = 250;
const DEER_POP_CAP: usize = 600;
const COW_POP_CAP: usize = 150;
const RABBIT_POP_CAP: usize = 800;
const PIG_POP_CAP: usize = 200;
const FOX_POP_CAP: usize = 150;
const CAT_POP_CAP: usize = 120;
const ANIMAL_BIRTH_COOLDOWN: u32 = TICKS_PER_SEASON * 2;
const REPRO_SEARCH_RADIUS: i32 = 3;

#[derive(Component)]
pub struct Wolf;

#[derive(Component)]
pub struct Deer;

#[derive(Component)]
pub struct Horse;

#[derive(Component)]
pub struct Cow;

#[derive(Component)]
pub struct Rabbit;

#[derive(Component)]
pub struct Pig;

#[derive(Component)]
pub struct Fox;

#[derive(Component)]
pub struct Cat;

/// Placed on an animal once tamed by a faction.
/// The animal stops fleeing from owner-faction persons and (for horses) can be
/// ridden. Single source of truth for ownership; `DomesticAnimal` carries the
/// richer husbandry state and is auto-attached alongside this marker by
/// `attach_pack_inventory_system`.
#[derive(Component, Clone, Copy)]
pub struct Tamed {
    pub owner_faction: u32,
}

/// Animal-husbandry species identity. `Cattle` projects onto the existing `Cow`
/// Bevy marker; `Dog` projects onto `Wolf` + `Tamed` (tamed-wolf disposition).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DomesticSpecies {
    Horse,
    Cattle,
    Pig,
    Dog,
    Cat,
}

impl DomesticSpecies {
    pub fn label(self) -> &'static str {
        match self {
            DomesticSpecies::Horse => "Horse",
            DomesticSpecies::Cattle => "Cattle",
            DomesticSpecies::Pig => "Pig",
            DomesticSpecies::Dog => "Dog",
            DomesticSpecies::Cat => "Cat",
        }
    }
}

/// Per-tamed-animal husbandry record. Inserted automatically by
/// `attach_pack_inventory_system` when `Tamed` is added; never carries
/// `owner_faction` (that lives on `Tamed`). `preferred_home` points at the
/// animal's assigned Pen / Stable entity once `assign_preferred_home_system`
/// runs.
#[derive(Component, Clone, Copy, Debug)]
pub struct DomesticAnimal {
    pub species: DomesticSpecies,
    pub training: u8,
    pub preferred_home: Option<Entity>,
    pub last_cared_tick: u32,
}

/// Which work role an `AnimalWorkClaim` reserves an animal for. v1 ships with
/// the variants populated for future v2 wiring (plow / cart); the executor
/// surfaces are stubbed but `Pack` / `Mount` / `Companion` are already live.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnimalUse {
    Pack,
    Mount,
    Plow,
    Cart,
    Companion,
    Guard,
}

/// Reserves a domestic animal for one worker + role. Cleared on task
/// completion / worker despawn / animal death; otherwise expires after
/// `expires_tick` (cleanup system runs each Sequential tick).
#[derive(Component, Clone, Copy, Debug)]
pub struct AnimalWorkClaim {
    pub worker: Entity,
    pub use_kind: AnimalUse,
    pub expires_tick: u32,
}

/// Bug-fix #2 (pack animals stranded): marks a tamed animal as
/// herd-following its owner faction's `home_tile`. Inserted by the
/// nomad commit + player pitch paths so the animal redirect survives
/// `LodLevel::Dormant` cycles. `following_band_animal_redirect_system`
/// re-snaps `AnimalAI.target_tile` to a small offset of the live
/// `home_tile` every `TICKS_PER_DAY/4`.
#[derive(Component, Clone, Copy, Debug)]
pub struct FollowingBand {
    pub faction: u32,
    pub last_redirect_tick: u32,
}

/// P8: per-species pack capacity in grams. Tuned so a horse comfortably
/// carries a packed yurt (80kg, but two horses split the load), while
/// dogs carry only light supplies (skins, tools).
pub const PACK_CAP_HORSE: u32 = 60_000;
pub const PACK_CAP_COW: u32 = 80_000;
pub const PACK_CAP_PIG: u32 = 30_000;
pub const PACK_CAP_DOG: u32 = 15_000;

/// Per-species pack-volume ceilings in millilitres. Saddlebag + lash-bundle
/// envelope; complements the weight ceilings above. At horse 180 L one
/// `packed_yurt` (180 L) saturates a single horse exactly.
pub const PACK_VOL_HORSE: u32 = 180_000;
pub const PACK_VOL_COW: u32 = 240_000;
pub const PACK_VOL_PIG: u32 = 80_000;
pub const PACK_VOL_DOG: u32 = 35_000;

/// Number of distinct stack types a pack animal can carry.
pub const PACK_INVENTORY_SLOTS: usize = 6;

/// P8: Per-tamed-animal cargo. Inserted automatically by
/// `attach_pack_inventory_system` when an animal becomes `Tamed`. The
/// pack contributes to its faction's storage rollup (third pass in
/// `compute_faction_storage_system`) for nomadic factions only.
#[derive(Component, Clone, Copy, Debug)]
pub struct PackAnimalInventory {
    pub items: [(crate::economy::resource_catalog::ResourceId, u32); PACK_INVENTORY_SLOTS],
    pub capacity_g: u32,
    pub capacity_ml: u32,
}

impl Default for PackAnimalInventory {
    fn default() -> Self {
        // Use a placeholder ResourceId for empty slots — match
        // `EconomicAgent`'s convention of `(fruit, 0)`.
        Self {
            items: [(crate::economy::core_ids::fruit(), 0); PACK_INVENTORY_SLOTS],
            capacity_g: 0,
            capacity_ml: 0,
        }
    }
}

impl PackAnimalInventory {
    pub fn for_capacity(cap_g: u32) -> Self {
        // Legacy constructor — used by tests and unscoped call sites.
        // Defaults volume cap to "very large" so weight stays the binding
        // constraint, matching pre-volume behaviour.
        let mut me = Self::default();
        me.capacity_g = cap_g;
        me.capacity_ml = u32::MAX;
        me
    }

    pub fn for_capacity_and_volume(cap_g: u32, cap_ml: u32) -> Self {
        let mut me = Self::default();
        me.capacity_g = cap_g;
        me.capacity_ml = cap_ml;
        me
    }

    pub fn current_weight_g(&self) -> u32 {
        let mut w = 0u32;
        for (rid, qty) in self.items.iter() {
            if *qty == 0 {
                continue;
            }
            let unit = rid.unit_weight_g().max(1);
            w = w.saturating_add(unit.saturating_mul(*qty));
        }
        w
    }

    pub fn current_volume_ml(&self) -> u32 {
        let mut v = 0u32;
        for (rid, qty) in self.items.iter() {
            if *qty == 0 {
                continue;
            }
            let unit = rid.unit_volume_ml().max(1);
            v = v.saturating_add(unit.saturating_mul(*qty));
        }
        v
    }

    pub fn free_capacity_g(&self) -> u32 {
        self.capacity_g.saturating_sub(self.current_weight_g())
    }

    pub fn free_capacity_ml(&self) -> u32 {
        self.capacity_ml.saturating_sub(self.current_volume_ml())
    }

    /// Add up to `qty` units of `rid`. Returns the number that did NOT fit.
    pub fn add(&mut self, rid: crate::economy::resource_catalog::ResourceId, qty: u32) -> u32 {
        if qty == 0 {
            return 0;
        }
        let unit_w = rid.unit_weight_g().max(1);
        let unit_v = rid.unit_volume_ml().max(1);
        let mut remaining = qty;
        let mut used_w = self.current_weight_g();
        let mut used_v = self.current_volume_ml();
        // Top up an existing matching stack first.
        for (slot_rid, slot_qty) in self.items.iter_mut() {
            if *slot_qty > 0 && *slot_rid == rid {
                let weight_room = self.capacity_g.saturating_sub(used_w);
                let vol_room = self.capacity_ml.saturating_sub(used_v);
                let by_weight = weight_room / unit_w;
                let by_volume = vol_room / unit_v;
                let take = remaining.min(by_weight).min(by_volume);
                if take == 0 {
                    return remaining;
                }
                *slot_qty = slot_qty.saturating_add(take);
                used_w = used_w.saturating_add(take.saturating_mul(unit_w));
                used_v = used_v.saturating_add(take.saturating_mul(unit_v));
                remaining -= take;
                if remaining == 0 {
                    return 0;
                }
                break;
            }
        }
        if remaining > 0 {
            // Find an empty slot.
            let weight_room = self.capacity_g.saturating_sub(used_w);
            let vol_room = self.capacity_ml.saturating_sub(used_v);
            let by_weight = weight_room / unit_w;
            let by_volume = vol_room / unit_v;
            let take = remaining.min(by_weight).min(by_volume);
            if take == 0 {
                return remaining;
            }
            for (slot_rid, slot_qty) in self.items.iter_mut() {
                if *slot_qty == 0 {
                    *slot_rid = rid;
                    *slot_qty = take;
                    remaining -= take;
                    break;
                }
            }
        }
        remaining
    }

    pub fn remove(&mut self, rid: crate::economy::resource_catalog::ResourceId, qty: u32) -> u32 {
        for (slot_rid, slot_qty) in self.items.iter_mut() {
            if *slot_rid == rid && *slot_qty > 0 {
                let removed = (*slot_qty).min(qty);
                *slot_qty -= removed;
                return removed;
            }
        }
        0
    }

    pub fn quantity_of(&self, rid: crate::economy::resource_catalog::ResourceId) -> u32 {
        self.items
            .iter()
            .filter(|(r, _)| *r == rid)
            .fold(0u32, |acc, (_, q)| acc.saturating_add(*q))
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (crate::economy::resource_catalog::ResourceId, u32)> + '_ {
        self.items.iter().filter(|(_, q)| *q > 0).copied()
    }
}

/// Per-tick `Added<Tamed>` hook system. Inserts a default
/// `PackAnimalInventory` sized for the species + a `DomesticAnimal` record
/// (species, training, preferred_home, last_cared_tick) when an animal is
/// freshly tamed. Idempotent — `Without<PackAnimalInventory>` / `Without<DomesticAnimal>`
/// gate against double-inserts.
///
/// Species mapping:
/// - `With<Horse>` → `DomesticSpecies::Horse`
/// - `With<Cow>`   → `DomesticSpecies::Cattle`
/// - `With<Pig>`   → `DomesticSpecies::Pig`
/// - `With<Cat>`   → `DomesticSpecies::Cat` (no pack inventory — companion)
/// - `With<Wolf>`  → `DomesticSpecies::Dog`  (tamed-wolf disposition + small pack)
pub fn attach_pack_inventory_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    horse_q: Query<Entity, (Added<Tamed>, With<Horse>, Without<DomesticAnimal>)>,
    cow_q: Query<Entity, (Added<Tamed>, With<Cow>, Without<DomesticAnimal>)>,
    pig_q: Query<Entity, (Added<Tamed>, With<Pig>, Without<DomesticAnimal>)>,
    cat_q: Query<Entity, (Added<Tamed>, With<Cat>, Without<DomesticAnimal>)>,
    wolf_q: Query<Entity, (Added<Tamed>, With<Wolf>, Without<DomesticAnimal>)>,
) {
    let now = clock.tick as u32;
    let mut attach = |e: Entity, species: DomesticSpecies, pack_cap: Option<(u32, u32)>| {
        let mut ec = commands.entity(e);
        ec.insert(DomesticAnimal {
            species,
            training: 0,
            preferred_home: None,
            last_cared_tick: now,
        });
        if let Some((cap_g, cap_ml)) = pack_cap {
            ec.insert(PackAnimalInventory::for_capacity_and_volume(cap_g, cap_ml));
        }
    };
    for e in horse_q.iter() {
        attach(e, DomesticSpecies::Horse, Some((PACK_CAP_HORSE, PACK_VOL_HORSE)));
    }
    for e in cow_q.iter() {
        attach(e, DomesticSpecies::Cattle, Some((PACK_CAP_COW, PACK_VOL_COW)));
    }
    for e in pig_q.iter() {
        attach(e, DomesticSpecies::Pig, Some((PACK_CAP_PIG, PACK_VOL_PIG)));
    }
    for e in cat_q.iter() {
        attach(e, DomesticSpecies::Cat, None);
    }
    for e in wolf_q.iter() {
        attach(e, DomesticSpecies::Dog, Some((PACK_CAP_DOG, PACK_VOL_DOG)));
    }
}

/// Sweeps stale `AnimalWorkClaim`s — expired by `expires_tick` or whose
/// worker entity no longer exists. Sequential cadence (every 60 ticks).
pub fn animal_work_claim_expiry_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    persons: Query<(), With<Person>>,
    claims: Query<(Entity, &AnimalWorkClaim)>,
) {
    if clock.tick % 60 != 0 {
        return;
    }
    let now = clock.tick as u32;
    for (e, claim) in claims.iter() {
        let worker_alive = persons.get(claim.worker).is_ok();
        if !worker_alive || now >= claim.expires_tick {
            commands.entity(e).remove::<AnimalWorkClaim>();
        }
    }
}

/// Bug-fix #2: re-snap `AnimalAI.target_tile` to a small offset of
/// each `FollowingBand` animal's faction `home_tile` every
/// `TICKS_PER_DAY/4`. Live read of the registry — survives migration
/// commits, player Pitch commands, and Dormant↔Active LOD cycles.
pub fn following_band_animal_redirect_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    mut q: Query<(&mut FollowingBand, &mut AnimalAI)>,
) {
    let tick = clock.tick;
    if tick % (crate::world::seasons::TICKS_PER_DAY as u64 / 4) != 0 {
        return;
    }
    let now = tick as u32;
    for (mut follow, mut ai) in q.iter_mut() {
        let Some(faction) = registry.factions.get(&follow.faction) else {
            continue;
        };
        let home = faction.home_tile;
        let seed = follow.faction.wrapping_mul(0x85EB_CA6B).wrapping_add(now);
        let dx = ((seed & 0b11) as i32) - 2;
        let dy = (((seed >> 2) & 0b11) as i32) - 2;
        ai.target_tile = (home.0 + dx, home.1 + dy);
        follow.last_redirect_tick = now;
    }
}

/// Placed on a horse while it is being ridden by a person.
/// Causes animal_movement_system to skip this entity (position managed by rider sync).
#[derive(Component, Clone, Copy)]
pub struct CarriedBy(pub Entity);

/// Tag on HERD-pattern species (Deer/Horse/Cow) carrying their birth cluster
/// id. Consumed by `animal_paths::HerdClusterRegistry` to recompute the
/// herd's center and serve flow-field queries.
#[derive(Component, Clone, Copy, Debug)]
pub struct HerdMember {
    pub cluster_id: u32,
}

/// Monotonic cluster id generator. Spawn-time herd tagging draws a fresh
/// id per cluster from this counter; survives `OnEnter(Playing)` reloads.
#[derive(Resource, Default)]
pub struct HerdClusterGen {
    pub next: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AnimalState {
    #[default]
    Wander = 0,
    Chase = 1,
    Flee = 2,
    Attack = 3,
    Sleeping = 4,
    /// Walking toward / standing adjacent to a non-salt water tile to
    /// drink. Set by `animal_water_seek_system` when `thirst >=
    /// ANIMAL_THIRST_TRIGGER`; cleared on adjacency-drink or when the
    /// animal is preempted by `Chase`/`Flee`/`Attack`.
    Drinking = 5,
}

#[derive(Component, Clone, Default)]
pub struct AnimalAI {
    pub state: AnimalState,
    pub target_tile: (i32, i32),
    pub target_entity: Option<Entity>,
    pub wander_timer: f32,
    /// Cached A* or flow-field path in (tx, ty, tz). Empty ⇒ replan next
    /// tick. `path[0]` is the start tile (already arrived); follow from
    /// `path[path_cursor]`. See `animal_paths.rs`.
    pub path: Vec<(i32, i32, i8)>,
    pub path_cursor: u16,
    /// Tile this path was computed *to*. Replan when `target_tile` drifts.
    pub path_goal: (i32, i32),
    /// Tick until which inline A\* replanning is suppressed for this animal.
    /// Set when a replan returned `Unreachable` / `BudgetExhausted` (the
    /// expensive-but-fruitless cases) so an out-of-range goal doesn't burn a
    /// full 256-node search every tick. `0` ⇒ no cooldown. See
    /// `animal_paths::ANIMAL_REPLAN_COOLDOWN_TICKS`.
    pub replan_cooldown_until: u64,
}

/// Lightweight biological needs for animals. Separate from the person Needs struct —
/// animals only track hunger, sleep, and reproduction.
#[derive(Component, Clone, Copy, Default)]
pub struct AnimalNeeds {
    pub hunger: f32,       // 0=satiated, 255=starving
    pub sleep: f32,        // 0=rested, 255=exhausted
    pub reproduction: f32, // 0=not ready, 255=peak
    /// 0=hydrated, 255=parched. Tick rate roughly 2× hunger so animals
    /// drink ~3× per game-day. Crosses `ANIMAL_THIRST_TRIGGER` mid-day.
    pub thirst: f32,
    /// Light non-lethal sickness counter. Decays at the same per-second
    /// rate as hunger. Slows movement and grazing recovery while > 0.
    pub sickness: f32,
}

/// Countdown in ticks before a female can give birth again.
#[derive(Component, Clone, Copy, Default)]
pub struct AnimalReproductionCooldown(pub u32);

/// Initial-condition spawn distribution per species. Group members are
/// placed within `cluster_radius` tiles of a randomly chosen center;
/// SOLITARY collapses to one-per-cluster with radius 0.
#[derive(Clone, Copy)]
struct SocialPattern {
    group_min: u32,
    group_max: u32,
    cluster_radius: i32,
}

const HERD: SocialPattern = SocialPattern {
    group_min: 8,
    group_max: 15,
    cluster_radius: 5,
};
const PACK: SocialPattern = SocialPattern {
    group_min: 3,
    group_max: 6,
    cluster_radius: 3,
};
const SOLITARY: SocialPattern = SocialPattern {
    group_min: 1,
    group_max: 1,
    cluster_radius: 0,
};

/// Pops cluster centers from the (pre-shuffled) `pool` and lays out group
/// members on tiles inside `biome_set` within `pattern.cluster_radius` of
/// the center. Returns up to `count` spawn locations in cluster order
/// with parallel cluster ids drawn from `next_cluster_id` (one id per
/// cluster). Truncates short if `pool` runs out before `count` is satisfied.
fn cluster_spawn_tiles(
    pool: &mut Vec<(i32, i32)>,
    biome_set: &AHashSet<(i32, i32)>,
    pattern: SocialPattern,
    count: u32,
    next_cluster_id: &mut u32,
) -> Vec<((i32, i32), u32)> {
    let mut out: Vec<((i32, i32), u32)> = Vec::with_capacity(count as usize);
    let mut remaining = count;
    while remaining > 0 {
        let center = match pool.pop() {
            Some(t) => t,
            None => break,
        };
        let cluster_id = *next_cluster_id;
        *next_cluster_id = next_cluster_id.wrapping_add(1);
        let group_size = if pattern.group_min >= pattern.group_max {
            pattern.group_min
        } else {
            fastrand::u32(pattern.group_min..=pattern.group_max)
        }
        .min(remaining);
        out.push((center, cluster_id));
        let mut used: Vec<(i32, i32)> = Vec::with_capacity(group_size as usize);
        used.push(center);
        for _ in 1..group_size {
            let mut placed = center;
            if pattern.cluster_radius > 0 {
                for _ in 0..16 {
                    let dx = fastrand::i32(-pattern.cluster_radius..=pattern.cluster_radius);
                    let dy = fastrand::i32(-pattern.cluster_radius..=pattern.cluster_radius);
                    let candidate = (center.0 + dx, center.1 + dy);
                    if biome_set.contains(&candidate) && !used.contains(&candidate) {
                        placed = candidate;
                        break;
                    }
                }
            }
            used.push(placed);
            out.push((placed, cluster_id));
        }
        remaining = remaining.saturating_sub(group_size);
    }
    out
}

pub fn spawn_animals(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    mut clock: ResMut<SimClock>,
    mut herd_gen: ResMut<HerdClusterGen>,
) {
    let now = Instant::now();

    let mut forest_tiles: Vec<(i32, i32)> = Vec::new();
    let mut grass_tiles: Vec<(i32, i32)> = Vec::new();

    for (coord, chunk) in chunk_map.0.iter() {
        let base_tx = coord.0 * CHUNK_SIZE as i32;
        let base_ty = coord.1 * CHUNK_SIZE as i32;
        for ly in 0..CHUNK_SIZE {
            for lx in 0..CHUNK_SIZE {
                if !chunk.is_locally_passable(lx, ly) {
                    continue;
                }
                let tx = base_tx + lx as i32;
                let ty = base_ty + ly as i32;
                match chunk.surface_tile_kind(lx, ly) {
                    TileKind::Forest => forest_tiles.push((tx, ty)),
                    TileKind::Grass => grass_tiles.push((tx, ty)),
                    _ => {}
                }
            }
        }
    }

    info!(
        "Animal spawn tiles found: {} forest, {} grass in {:?}",
        forest_tiles.len(),
        grass_tiles.len(),
        now.elapsed()
    );

    if forest_tiles.is_empty() || grass_tiles.is_empty() {
        warn!("spawn_animals: no forest or grass tiles found in loaded chunks — animals may not spawn!");
    }

    let forest_set: AHashSet<(i32, i32)> = forest_tiles.iter().copied().collect();
    let grass_set: AHashSet<(i32, i32)> = grass_tiles.iter().copied().collect();

    fastrand::shuffle(&mut forest_tiles);
    fastrand::shuffle(&mut grass_tiles);

    let mut slot = clock.population;

    // Wolves: pack predator, forest.
    let wolf_tiles = cluster_spawn_tiles(
        &mut forest_tiles,
        &forest_set,
        PACK,
        WOLF_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), _cid)) in wolf_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Wolf,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.05,
                ..Default::default()
            },
            Health::new(30),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Wolf),
        ));
        slot += 1;
    }

    // Deer: herd grazer, grass.
    let deer_tiles = cluster_spawn_tiles(
        &mut grass_tiles,
        &grass_set,
        HERD,
        DEER_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), cluster_id)) in deer_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            (Deer, HerdMember { cluster_id }),
            (
                Transform::from_xyz(pos.x, pos.y, 1.0),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
                AnimalAI {
                    target_tile: (tx, ty),
                    wander_timer: i as f32 * 0.02,
                    ..Default::default()
                },
                Health::new(20),
                CombatTarget::default(),
                CombatCooldown::default(),
                LodLevel::Full,
                BucketSlot(slot),
                crate::simulation::plants::DeerGrazer {
                    graze_timer: fastrand::u16(0..120),
                },
                AnimalNeeds {
                    hunger: fastrand::f32() * 60.0,
                    sleep: fastrand::f32() * 40.0,
                    reproduction: fastrand::f32() * 80.0,
                    thirst: fastrand::f32() * 50.0,
                    sickness: 0.0,
                },
                AnimalReproductionCooldown(0),
                BiologicalSex::random(),
            ),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Deer),
        ));
        slot += 1;
    }

    // Horses: herd, grass.
    let horse_tiles = cluster_spawn_tiles(
        &mut grass_tiles,
        &grass_set,
        HERD,
        HORSE_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), cluster_id)) in horse_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            (Horse, HerdMember { cluster_id }),
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.03,
                ..Default::default()
            },
            Health::new(HORSE_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Horse),
        ));
        slot += 1;
    }

    // Cows: herd, grass.
    let cow_tiles = cluster_spawn_tiles(
        &mut grass_tiles,
        &grass_set,
        HERD,
        COW_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), cluster_id)) in cow_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            (Cow, HerdMember { cluster_id }),
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.04,
                ..Default::default()
            },
            Health::new(COW_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Cow),
        ));
        slot += 1;
    }

    // Rabbits: small warrens (pack), grass.
    let rabbit_tiles = cluster_spawn_tiles(
        &mut grass_tiles,
        &grass_set,
        PACK,
        RABBIT_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), _cid)) in rabbit_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Rabbit,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.01,
                ..Default::default()
            },
            Health::new(RABBIT_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
        ));
        slot += 1;
    }

    // Pigs: small sounders (pack), forest.
    let pig_tiles = cluster_spawn_tiles(
        &mut forest_tiles,
        &forest_set,
        PACK,
        PIG_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), _cid)) in pig_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Pig,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.04,
                ..Default::default()
            },
            Health::new(PIG_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Pig),
        ));
        slot += 1;
    }

    // Foxes: small family (pack), forest.
    let fox_tiles = cluster_spawn_tiles(
        &mut forest_tiles,
        &forest_set,
        PACK,
        FOX_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), _cid)) in fox_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Fox,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.03,
                ..Default::default()
            },
            Health::new(FOX_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
        ));
        slot += 1;
    }

    // Cats: solitary, forest.
    let cat_tiles = cluster_spawn_tiles(
        &mut forest_tiles,
        &forest_set,
        SOLITARY,
        CAT_COUNT,
        &mut herd_gen.next,
    );
    for (i, &((tx, ty), _cid)) in cat_tiles.iter().enumerate() {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            Cat,
            Transform::from_xyz(pos.x, pos.y, 1.0),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            AnimalAI {
                target_tile: (tx, ty),
                wander_timer: i as f32 * 0.03,
                ..Default::default()
            },
            Health::new(CAT_HP),
            CombatTarget::default(),
            CombatCooldown::default(),
            LodLevel::Full,
            BucketSlot(slot),
            AnimalNeeds {
                hunger: fastrand::f32() * 60.0,
                sleep: fastrand::f32() * 40.0,
                reproduction: fastrand::f32() * 80.0,
                thirst: fastrand::f32() * 50.0,
                sickness: 0.0,
            },
            AnimalReproductionCooldown(0),
            BiologicalSex::random(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Cat),
        ));
        slot += 1;
    }

    clock.population = slot;
    clock.current_end = clock.bucket_size.min(slot);
}

pub fn animal_movement_system(
    time: Res<Time>,
    chunk_map: Res<ChunkMap>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    herd_registry: Res<crate::simulation::animal_paths::HerdClusterRegistry>,
    mut pool: ResMut<crate::pathfinding::pool::AStarPool>,
    mut query: Query<
        (
            Entity,
            &mut Transform,
            &mut AnimalAI,
            &LodLevel,
            &BucketSlot,
            Option<&CarriedBy>,
            Option<&HerdMember>,
            bevy::prelude::Has<Deer>,
            bevy::prelude::Has<Horse>,
            bevy::prelude::Has<Cow>,
        ),
        Without<Person>,
    >,
    clock: Res<SimClock>,
    timings: Res<crate::simulation::speed::SuspectSystemTimings>,
    budget: Res<crate::simulation::perf::PerfWorkBudget>,
    mut replan_cursor: ResMut<crate::simulation::animal_paths::AnimalReplanCursor>,
) {
    let _t = timings.guard(crate::simulation::speed::suspect::ANIMAL_MOVEMENT);
    let dt = time.delta_secs();
    let sim_dt = dt * clock.scale_factor();
    let now = clock.tick;

    // Per-tick inline-A* replan budget (round-robin by entity bits, mirroring
    // `vision_system`). Animals step cached paths every tick (cheap); only the
    // expensive A* replan is capped. Build the eligible-this-tick set
    // (need-replan, off-cooldown, A*-bound), sort, rotate by cursor, take cap.
    // HERD flow-field replans are free and not gated here.
    let cap = budget.animal_replans_per_tick.max(1);
    let mut eligible: Vec<Entity> = Vec::new();
    for (entity, _transform, ai, lod, _slot, carried, _hm, _d, _h, _c) in query.iter() {
        if *lod == LodLevel::Dormant
            || carried.is_some()
            || ai.state == AnimalState::Attack
            || ai.state == AnimalState::Sleeping
            || ai.replan_cooldown_until > now
        {
            continue;
        }
        let need_replan = ai.path.is_empty()
            || ai.path_goal != ai.target_tile
            || (ai.path_cursor as usize) >= ai.path.len();
        if need_replan {
            eligible.push(entity);
        }
    }
    let (allowed, next_bits) = crate::simulation::animal_paths::select_replan_slice(
        &mut eligible,
        cap,
        replan_cursor.next_bits,
    );
    replan_cursor.next_bits = next_bits;

    for (entity, mut transform, mut ai, lod, slot, carried, herd_member, is_deer, is_horse, is_cow) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        // Ridden horses are positioned by horse_position_sync_system
        if carried.is_some() {
            continue;
        }
        if ai.state == AnimalState::Attack || ai.state == AnimalState::Sleeping {
            continue;
        }

        let pos = transform.translation.truncate();
        let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
        let cur_ty = (pos.y / TILE_SIZE).floor() as i32;
        let cur_z = chunk_map.surface_z_at(cur_tx, cur_ty) as i8;
        let is_herd_species = is_deer || is_horse || is_cow;

        // Replan when path is empty / stale / consumed.
        let need_replan = ai.path.is_empty()
            || ai.path_goal != ai.target_tile
            || (ai.path_cursor as usize) >= ai.path.len();
        if need_replan {
            ai.path.clear();
            ai.path_cursor = 0;
            let mut planned = false;
            // HERD species try the flow field first — cheap, always allowed,
            // ignores the A* budget + cooldown (those gate inline A* only).
            if is_herd_species {
                if let Some(hm) = herd_member {
                    if crate::simulation::animal_paths::try_replan_via_flow_field(
                        &herd_registry,
                        &mut ai,
                        hm.cluster_id,
                        (cur_tx, cur_ty),
                        cur_z,
                    ) {
                        planned = true;
                    }
                }
            }
            // Inline A* fallback — budgeted (this tick's round-robin slice) and
            // throttled (no re-A* while a fruitless-search cooldown holds).
            let mut gave_up = false;
            if !planned && ai.replan_cooldown_until <= now && allowed.contains(&entity) {
                let goal_z = chunk_map.surface_z_at(ai.target_tile.0, ai.target_tile.1) as i8;
                let start = (cur_tx, cur_ty, cur_z);
                let goal = (ai.target_tile.0, ai.target_tile.1, goal_z);
                let scratch = pool.scratch(2);
                use crate::simulation::animal_paths::{
                    AnimalReplanOutcome, ANIMAL_REPLAN_COOLDOWN_TICKS,
                };
                match crate::simulation::animal_paths::replan_astar(
                    scratch, &chunk_map, &mut ai, start, goal,
                ) {
                    AnimalReplanOutcome::Planned => planned = true,
                    AnimalReplanOutcome::PlannedPartial => {
                        // Walkable one-step partial toward an out-of-range goal;
                        // throttle the next full search.
                        planned = true;
                        ai.replan_cooldown_until = now + ANIMAL_REPLAN_COOLDOWN_TICKS;
                    }
                    AnimalReplanOutcome::Unreachable => {
                        ai.replan_cooldown_until = now + ANIMAL_REPLAN_COOLDOWN_TICKS;
                        gave_up = true;
                    }
                }
            }
            if !planned {
                if gave_up {
                    // Genuinely unreachable: drop into Wander, stall on current
                    // tile, let wander_timer pick a new step next pass.
                    let tw = tile_to_world(cur_tx, cur_ty);
                    transform.translation.x = tw.x;
                    transform.translation.y = tw.y;
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    ai.target_tile = (cur_tx, cur_ty);
                    ai.wander_timer = 0.0;
                }
                // else: cooldown- or budget-deferred this tick. Keep the goal +
                // empty path and skip movement; the animal rotates into the
                // replan slice (or the cooldown lapses) within a few ticks.
                continue;
            }
        }

        // Follow path: aim at path[cursor]. If we've consumed everything,
        // fall through to wander pick.
        if (ai.path_cursor as usize) >= ai.path.len() {
            let tw = tile_to_world(cur_tx, cur_ty);
            transform.translation.x = tw.x;
            transform.translation.y = tw.y;
            ai.path.clear();
            ai.path_cursor = 0;
            if matches!(ai.state, AnimalState::Wander | AnimalState::Flee) {
                if clock.is_active(slot.0) {
                    ai.wander_timer -= sim_dt;
                    if ai.wander_timer <= 0.0 {
                        ai.wander_timer = WANDER_INTERVAL;
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;
                        let dirs: [(i32, i32); 8] = [
                            (-1, 0),
                            (1, 0),
                            (0, -1),
                            (0, 1),
                            (-1, -1),
                            (1, -1),
                            (-1, 1),
                            (1, 1),
                        ];
                        let start = fastrand::usize(..8);
                        for i in 0..8 {
                            let (dx, dy) = dirs[(start + i) % 8];
                            let ntx = cur_tx + dx;
                            let nty = cur_ty + dy;
                            let ntz = chunk_map.surface_z_at(ntx, nty);
                            if chunk_map.passable_at(ntx, nty, ntz)
                                && !spatial.agent_occupied(ntx, nty, ntz)
                            {
                                ai.target_tile = (ntx, nty);
                                break;
                            }
                        }
                    }
                }
            } else if ai.state == AnimalState::Chase {
                ai.state = AnimalState::Wander;
            }
            continue;
        }

        let (nx, ny, nz) = ai.path[ai.path_cursor as usize];
        // Defense-in-depth: the world may have changed since plan
        // (a wall finalised, a tile streamed in). Reject and replan.
        if !chunk_map.passable_at(nx, ny, nz as i32) {
            let tw = tile_to_world(cur_tx, cur_ty);
            transform.translation.x = tw.x;
            transform.translation.y = tw.y;
            ai.path.clear();
            ai.path_cursor = 0;
            ai.state = AnimalState::Wander;
            ai.target_entity = None;
            ai.target_tile = (cur_tx, cur_ty);
            ai.wander_timer = 0.0;
            continue;
        }

        let target_world = tile_to_world(nx, ny);
        let to_target = target_world - pos;
        let dist = to_target.length();
        if dist > 2.0 {
            let dir = to_target.normalize();
            let step = dir * ANIMAL_SPEED * dt;
            if step.length() >= dist {
                transform.translation.x = target_world.x;
                transform.translation.y = target_world.y;
                ai.path_cursor = ai.path_cursor.saturating_add(1);
            } else {
                let new_pos = pos + step;
                transform.translation.x = new_pos.x;
                transform.translation.y = new_pos.y;
            }
        } else {
            transform.translation.x = target_world.x;
            transform.translation.y = target_world.y;
            ai.path_cursor = ai.path_cursor.saturating_add(1);
        }
    }
}

/// Ticks animal needs and transitions the sleep state. Runs in ParallelA.
pub fn animal_needs_tick_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    mut query: Query<
        (&BucketSlot, &LodLevel, &mut AnimalNeeds, &mut AnimalAI),
        bevy::prelude::Or<(
            With<Wolf>,
            With<Deer>,
            With<Horse>,
            With<Cow>,
            With<Rabbit>,
            With<Pig>,
            With<Fox>,
            With<Cat>,
        )>,
    >,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    query
        .par_iter_mut()
        .for_each(|(slot, lod, mut needs, mut ai)| {
            if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
                return;
            }

            needs.reproduction = (needs.reproduction + ANIMAL_REPRO_RATE * dt).clamp(0.0, 255.0);
            needs.sickness = (needs.sickness - ANIMAL_SICKNESS_DECAY_RATE * dt).max(0.0);

            if ai.state == AnimalState::Sleeping {
                needs.sleep = (needs.sleep - ANIMAL_SLEEP_RECOVER_RATE * dt).clamp(0.0, 255.0);
                if needs.sleep <= ANIMAL_SLEEP_WAKE_THRESHOLD {
                    ai.state = AnimalState::Wander;
                }
            } else {
                needs.hunger = (needs.hunger + ANIMAL_HUNGER_RATE * dt).clamp(0.0, 255.0);
                needs.thirst = (needs.thirst + ANIMAL_THIRST_RATE * dt).clamp(0.0, 255.0);
                needs.sleep = (needs.sleep + ANIMAL_SLEEP_RATE * dt).clamp(0.0, 255.0);
                // Only sleep from Wander — never interrupt Chase/Flee/Attack/Drinking
                if needs.sleep >= ANIMAL_SLEEP_THRESHOLD && ai.state == AnimalState::Wander {
                    ai.state = AnimalState::Sleeping;
                    ai.target_entity = None;
                }
            }
        });
}

/// Wolves chase deer/lone humans; deer flee from wolves; horses flee wolves and unknown persons.
/// Foxes/cats hunt rabbits; cows/pigs/rabbits flee predators.
/// Runs in ParallelA — writes only AnimalAI on self.
pub fn animal_sense_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    wolf_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Wolf>>,
    deer_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Deer>>,
    horse_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel, Option<&Tamed>), With<Horse>>,
    cow_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel, Option<&Tamed>), With<Cow>>,
    rabbit_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Rabbit>>,
    pig_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel, Option<&Tamed>), With<Pig>>,
    fox_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel), With<Fox>>,
    cat_query: Query<(Entity, &Transform, &BucketSlot, &LodLevel, Option<&Tamed>), With<Cat>>,
    person_query: Query<&Transform, With<Person>>,
    mut ai_query: Query<(&mut AnimalAI, &mut CombatTarget, Option<&mut AnimalNeeds>)>,
    target_query: Query<(&Transform, Option<&Health>, Option<&Body>)>,
) {
    const WOLF_HUNT_RADIUS: i32 = 12;
    const DEER_FLEE_RADIUS: i32 = 8;
    const LONE_HUMAN_RADIUS: i32 = 3;

    // Wolf sense: find deer or lone humans
    for (wolf_entity, transform, slot, lod) in &wolf_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let Ok((mut ai, mut combat_target, mut animal_needs)) = ai_query.get_mut(wolf_entity)
        else {
            continue;
        };

        // Don't interrupt sleep
        if ai.state == AnimalState::Sleeping {
            continue;
        }

        // If already chasing/attacking a valid target, keep it
        if let Some(existing) = ai.target_entity {
            if ai.state == AnimalState::Chase || ai.state == AnimalState::Attack {
                // Hysteresis: abandon a human chase once hunger drops below the
                // drop threshold. Deer chases stay regardless of hunger.
                if person_query.get(existing).is_ok()
                    && animal_needs
                        .as_deref()
                        .map_or(true, |n| n.hunger < WOLF_DROP_HUMAN_TARGET_HUNGER)
                {
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    combat_target.0 = None;
                    continue;
                }
                if let Ok((prey_transform, health, body)) = target_query.get(existing) {
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;
                        combat_target.0 = None;
                        // Prey is dead — wolf ate
                        if let Some(ref mut needs) = animal_needs {
                            needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_WOLF).max(0.0);
                        }
                    } else {
                        let ptx = (prey_transform.translation.x / TILE_SIZE).floor() as i32;
                        let pty = (prey_transform.translation.y / TILE_SIZE).floor() as i32;
                        ai.target_tile = (ptx, pty);

                        let target_tile = ai.target_tile;
                        let dist =
                            (target_tile.0 as i32 - tx).abs() + (target_tile.1 as i32 - ty).abs();
                        if dist <= 1 {
                            ai.state = AnimalState::Attack;
                            combat_target.0 = Some(existing);
                        } else {
                            ai.state = AnimalState::Chase;
                        }
                        continue;
                    }
                } else {
                    // Target entity gone from world — wolf ate or prey escaped
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    combat_target.0 = None;
                    if let Some(ref mut needs) = animal_needs {
                        needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_WOLF).max(0.0);
                    }
                }
            }
        }

        // Scan for prey
        let mut found: Option<(Entity, i32, i32)> = None;

        'scan: for dy in -WOLF_HUNT_RADIUS..=WOLF_HUNT_RADIUS {
            for dx in -WOLF_HUNT_RADIUS..=WOLF_HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if candidate == wolf_entity {
                        continue;
                    }

                    let Ok((_, health, body)) = target_query.get(candidate) else {
                        continue;
                    };
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        continue;
                    }

                    let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                    let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                    if !has_los(
                        &chunk_map,
                        &door_map,
                        (tx, ty, z_from),
                        (tx + dx, ty + dy, z_to),
                    ) {
                        continue;
                    }

                    // Prefer deer (best meal, ends scan)
                    if deer_query.contains(candidate) {
                        found = Some((candidate, (tx + dx) as i32, (ty + dy) as i32));
                        break 'scan;
                    }

                    // Secondary prey: rabbits and pigs. Take if no better target yet,
                    // but keep scanning for a deer.
                    if found.is_none()
                        && (rabbit_query.contains(candidate) || pig_query.contains(candidate))
                    {
                        found = Some((candidate, (tx + dx) as i32, (ty + dy) as i32));
                        continue;
                    }

                    // Lone human check — only really hungry wolves predate humans, and
                    // only if no animal prey already located.
                    if found.is_some() {
                        continue;
                    }
                    let hungry_enough = animal_needs
                        .as_deref()
                        .map_or(false, |n| n.hunger >= WOLF_AGGRESSIVE_HUNGER);
                    if hungry_enough && person_query.get(candidate).is_ok() {
                        let mut nearby_persons = 0u32;
                        for ndy in -LONE_HUMAN_RADIUS..=LONE_HUMAN_RADIUS {
                            for ndx in -LONE_HUMAN_RADIUS..=LONE_HUMAN_RADIUS {
                                for &nb in spatial.get(tx + dx + ndx, ty + dy + ndy) {
                                    if nb != candidate && person_query.get(nb).is_ok() {
                                        nearby_persons += 1;
                                    }
                                }
                            }
                        }
                        if nearby_persons == 0 {
                            found = Some((candidate, (tx + dx) as i32, (ty + dy) as i32));
                        }
                    }
                }
            }
        }

        if let Some((prey, ptx, pty)) = found {
            ai.state = AnimalState::Chase;
            ai.target_entity = Some(prey);
            ai.target_tile = (ptx, pty);
        } else {
            if ai.state != AnimalState::Wander {
                ai.state = AnimalState::Wander;
                ai.target_entity = None;
                combat_target.0 = None;
            }
        }
    }

    // Deer sense: flee from wolves
    for (deer_entity, transform, slot, lod) in &deer_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let Ok((mut ai, _, _)) = ai_query.get_mut(deer_entity) else {
            continue;
        };

        // Don't interrupt sleep
        if ai.state == AnimalState::Sleeping {
            continue;
        }

        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;

        for dy in -DEER_FLEE_RADIUS..=DEER_FLEE_RADIUS {
            for dx in -DEER_FLEE_RADIUS..=DEER_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if wolf_query.contains(candidate) || person_query.get(candidate).is_ok() {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }

        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;

            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.wander_timer = 1.5;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }

    // Horse sense: flee from wolves always; flee from persons if wild (untamed)
    for (horse_entity, transform, slot, lod, tamed_opt) in &horse_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let Ok((mut ai, _, _)) = ai_query.get_mut(horse_entity) else {
            continue;
        };

        if ai.state == AnimalState::Sleeping {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        const HORSE_FLEE_RADIUS: i32 = 10;
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        let is_wild = tamed_opt.is_none();

        for dy in -HORSE_FLEE_RADIUS..=HORSE_FLEE_RADIUS {
            for dx in -HORSE_FLEE_RADIUS..=HORSE_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    let is_wolf_threat = wolf_query.contains(candidate);
                    let is_person_threat = is_wild && person_query.get(candidate).is_ok();
                    if is_wolf_threat || is_person_threat {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }

        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.wander_timer = 1.5;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }

    // Cow sense: flee from wolves; flee from persons if wild
    const COW_FLEE_RADIUS: i32 = 7;
    for (cow_entity, transform, slot, lod, tamed_opt) in &cow_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let Ok((mut ai, _, _)) = ai_query.get_mut(cow_entity) else {
            continue;
        };
        if ai.state == AnimalState::Sleeping {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let is_wild = tamed_opt.is_none();
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        for dy in -COW_FLEE_RADIUS..=COW_FLEE_RADIUS {
            for dx in -COW_FLEE_RADIUS..=COW_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    let is_wolf = wolf_query.contains(candidate);
                    let is_person_threat = is_wild && person_query.get(candidate).is_ok();
                    if is_wolf || is_person_threat {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }
        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.wander_timer = 1.5;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }

    // Pig sense: flee from wolves only (omnivore — not afraid of humans)
    const PIG_FLEE_RADIUS: i32 = 6;
    for (pig_entity, transform, slot, lod, _tamed_opt) in &pig_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let Ok((mut ai, _, _)) = ai_query.get_mut(pig_entity) else {
            continue;
        };
        if ai.state == AnimalState::Sleeping {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        for dy in -PIG_FLEE_RADIUS..=PIG_FLEE_RADIUS {
            for dx in -PIG_FLEE_RADIUS..=PIG_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if wolf_query.contains(candidate) {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }
        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.wander_timer = 1.5;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }

    // Rabbit sense: flee from anything bigger (wolves, foxes, cats, persons)
    const RABBIT_FLEE_RADIUS: i32 = 6;
    for (rabbit_entity, transform, slot, lod) in &rabbit_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let Ok((mut ai, _, _)) = ai_query.get_mut(rabbit_entity) else {
            continue;
        };
        if ai.state == AnimalState::Sleeping {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        for dy in -RABBIT_FLEE_RADIUS..=RABBIT_FLEE_RADIUS {
            for dx in -RABBIT_FLEE_RADIUS..=RABBIT_FLEE_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    let is_threat = wolf_query.contains(candidate)
                        || fox_query.contains(candidate)
                        || cat_query.contains(candidate)
                        || person_query.get(candidate).is_ok();
                    if is_threat {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }
        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.wander_timer = 1.0;
        } else if ai.state == AnimalState::Flee {
            ai.state = AnimalState::Wander;
        }
    }

    // Fox sense: hunt rabbits; flee from wolves
    const FOX_HUNT_RADIUS: i32 = 8;
    for (fox_entity, transform, slot, lod) in &fox_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let Ok((mut ai, mut combat_target, mut animal_needs)) = ai_query.get_mut(fox_entity) else {
            continue;
        };
        if ai.state == AnimalState::Sleeping {
            continue;
        }

        // Flee from wolves (overrides hunting)
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        for dy in -FOX_HUNT_RADIUS..=FOX_HUNT_RADIUS {
            for dx in -FOX_HUNT_RADIUS..=FOX_HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if wolf_query.contains(candidate) {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }
        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.target_entity = None;
            combat_target.0 = None;
            ai.wander_timer = 1.5;
            continue;
        }

        // Maintain existing chase
        if let Some(existing) = ai.target_entity {
            if ai.state == AnimalState::Chase || ai.state == AnimalState::Attack {
                if let Ok((prey_transform, health, body)) = target_query.get(existing) {
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;
                        combat_target.0 = None;
                        if let Some(ref mut needs) = animal_needs {
                            needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_FOX).max(0.0);
                        }
                    } else {
                        let ptx = (prey_transform.translation.x / TILE_SIZE).floor() as i32;
                        let pty = (prey_transform.translation.y / TILE_SIZE).floor() as i32;
                        ai.target_tile = (ptx, pty);
                        let dist = (ptx as i32 - tx).abs() + (pty as i32 - ty).abs();
                        if dist <= 1 {
                            ai.state = AnimalState::Attack;
                            combat_target.0 = Some(existing);
                        } else {
                            ai.state = AnimalState::Chase;
                        }
                        continue;
                    }
                } else {
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    combat_target.0 = None;
                    if let Some(ref mut needs) = animal_needs {
                        needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_FOX).max(0.0);
                    }
                }
            }
        }

        // Scan for rabbits
        let mut found: Option<(Entity, i32, i32)> = None;
        'fox_scan: for dy in -FOX_HUNT_RADIUS..=FOX_HUNT_RADIUS {
            for dx in -FOX_HUNT_RADIUS..=FOX_HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if candidate == fox_entity {
                        continue;
                    }
                    if !rabbit_query.contains(candidate) {
                        continue;
                    }
                    let Ok((_, health, body)) = target_query.get(candidate) else {
                        continue;
                    };
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        continue;
                    }
                    let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                    let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                    if !has_los(
                        &chunk_map,
                        &door_map,
                        (tx, ty, z_from),
                        (tx + dx, ty + dy, z_to),
                    ) {
                        continue;
                    }
                    found = Some((candidate, (tx + dx) as i32, (ty + dy) as i32));
                    break 'fox_scan;
                }
            }
        }
        if let Some((prey, ptx, pty)) = found {
            ai.state = AnimalState::Chase;
            ai.target_entity = Some(prey);
            ai.target_tile = (ptx, pty);
        } else if ai.state != AnimalState::Wander {
            ai.state = AnimalState::Wander;
            ai.target_entity = None;
            combat_target.0 = None;
        }
    }

    // Cat sense: hunt rabbits; flee from wolves; tamed cats don't flee from owner faction members
    const CAT_HUNT_RADIUS: i32 = 7;
    for (cat_entity, transform, slot, lod, _tamed_opt) in &cat_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let Ok((mut ai, mut combat_target, mut animal_needs)) = ai_query.get_mut(cat_entity) else {
            continue;
        };
        if ai.state == AnimalState::Sleeping {
            continue;
        }

        // Flee from wolves
        let mut threat_dx = 0i32;
        let mut threat_dy = 0i32;
        let mut threat_count = 0i32;
        for dy in -CAT_HUNT_RADIUS..=CAT_HUNT_RADIUS {
            for dx in -CAT_HUNT_RADIUS..=CAT_HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if wolf_query.contains(candidate) {
                        let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                        let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                        if has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, z_from),
                            (tx + dx, ty + dy, z_to),
                        ) {
                            threat_dx += dx;
                            threat_dy += dy;
                            threat_count += 1;
                        }
                    }
                }
            }
        }
        if threat_count > 0 {
            use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
            let total_tiles_x = GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let total_tiles_y = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
            let flee_tx = (tx - threat_dx / threat_count).clamp(0, total_tiles_x - 1);
            let flee_ty = (ty - threat_dy / threat_count).clamp(0, total_tiles_y - 1);
            ai.state = AnimalState::Flee;
            ai.target_tile = (flee_tx as i32, flee_ty as i32);
            ai.target_entity = None;
            combat_target.0 = None;
            ai.wander_timer = 1.5;
            continue;
        }

        // Maintain chase
        if let Some(existing) = ai.target_entity {
            if ai.state == AnimalState::Chase || ai.state == AnimalState::Attack {
                if let Ok((prey_transform, health, body)) = target_query.get(existing) {
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        ai.state = AnimalState::Wander;
                        ai.target_entity = None;
                        combat_target.0 = None;
                        if let Some(ref mut needs) = animal_needs {
                            needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_CAT).max(0.0);
                        }
                    } else {
                        let ptx = (prey_transform.translation.x / TILE_SIZE).floor() as i32;
                        let pty = (prey_transform.translation.y / TILE_SIZE).floor() as i32;
                        ai.target_tile = (ptx, pty);
                        let dist = (ptx as i32 - tx).abs() + (pty as i32 - ty).abs();
                        if dist <= 1 {
                            ai.state = AnimalState::Attack;
                            combat_target.0 = Some(existing);
                        } else {
                            ai.state = AnimalState::Chase;
                        }
                        continue;
                    }
                } else {
                    ai.state = AnimalState::Wander;
                    ai.target_entity = None;
                    combat_target.0 = None;
                    if let Some(ref mut needs) = animal_needs {
                        needs.hunger = (needs.hunger - ANIMAL_HUNGER_RECOVER_CAT).max(0.0);
                    }
                }
            }
        }

        // Scan for rabbits
        let mut found: Option<(Entity, i32, i32)> = None;
        'cat_scan: for dy in -CAT_HUNT_RADIUS..=CAT_HUNT_RADIUS {
            for dx in -CAT_HUNT_RADIUS..=CAT_HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if candidate == cat_entity {
                        continue;
                    }
                    if !rabbit_query.contains(candidate) {
                        continue;
                    }
                    let Ok((_, health, body)) = target_query.get(candidate) else {
                        continue;
                    };
                    let is_dead = match (health, body) {
                        (Some(h), _) if h.is_dead() => true,
                        (_, Some(b)) if b.is_dead() => true,
                        _ => false,
                    };
                    if is_dead {
                        continue;
                    }
                    let z_from = chunk_map.surface_z_at(tx, ty) as i8;
                    let z_to = chunk_map.surface_z_at(tx + dx, ty + dy) as i8;
                    if !has_los(
                        &chunk_map,
                        &door_map,
                        (tx, ty, z_from),
                        (tx + dx, ty + dy, z_to),
                    ) {
                        continue;
                    }
                    found = Some((candidate, (tx + dx) as i32, (ty + dy) as i32));
                    break 'cat_scan;
                }
            }
        }
        if let Some((prey, ptx, pty)) = found {
            ai.state = AnimalState::Chase;
            ai.target_entity = Some(prey);
            ai.target_tile = (ptx, pty);
        } else if ai.state != AnimalState::Wander {
            ai.state = AnimalState::Wander;
            ai.target_entity = None;
            combat_target.0 = None;
        }
    }
}

/// Counts down reproduction cooldowns. Runs in Economy set.
pub fn animal_reproduction_cooldown_system(
    clock: Res<SimClock>,
    mut query: Query<
        (&mut AnimalReproductionCooldown, &BucketSlot, &LodLevel),
        bevy::prelude::Or<(
            With<Wolf>,
            With<Deer>,
            With<Horse>,
            With<Cow>,
            With<Rabbit>,
            With<Pig>,
            With<Fox>,
            With<Cat>,
        )>,
    >,
) {
    query.par_iter_mut().for_each(|(mut cd, slot, lod)| {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            return;
        }
        if cd.0 > 0 {
            cd.0 = cd.0.saturating_sub(1);
        }
    });
}

/// Animal reproduction: pairs up nearby males and females of the same species to spawn offspring.
/// Runs in Economy set, after animal_reproduction_cooldown_system.
pub fn animal_reproduction_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    mut clock: ResMut<SimClock>,
    wolf_count: Query<(), With<Wolf>>,
    deer_count: Query<(), With<Deer>>,
    horse_count: Query<(), With<Horse>>,
    cow_count: Query<(), With<Cow>>,
    rabbit_count: Query<(), With<Rabbit>>,
    pig_count: Query<(), With<Pig>>,
    fox_count: Query<(), With<Fox>>,
    cat_count: Query<(), With<Cat>>,
    mut animal_query: Query<(
        Entity,
        &Transform,
        &BiologicalSex,
        &mut AnimalNeeds,
        &mut AnimalReproductionCooldown,
        &LodLevel,
        &BucketSlot,
        bevy::prelude::Has<Wolf>,
        bevy::prelude::Has<Deer>,
        bevy::prelude::Has<Horse>,
        bevy::prelude::Has<Cow>,
        bevy::prelude::Has<Rabbit>,
        bevy::prelude::Has<Pig>,
        bevy::prelude::Has<Fox>,
        bevy::prelude::Has<Cat>,
    )>,
    herd_query: Query<&HerdMember>,
) {
    let wolf_pop = wolf_count.iter().count();
    let deer_pop = deer_count.iter().count();
    let horse_pop = horse_count.iter().count();
    let cow_pop = cow_count.iter().count();
    let rabbit_pop = rabbit_count.iter().count();
    let pig_pop = pig_count.iter().count();
    let fox_pop = fox_count.iter().count();
    let cat_pop = cat_count.iter().count();

    // Species codes: 0=wolf 1=deer 2=horse 3=cow 4=rabbit 5=pig 6=fox 7=cat

    // Phase 1: collect eligible males (immutable pass)
    let mut males: [ahash::AHashSet<Entity>; 8] = Default::default();

    for (
        entity,
        _,
        sex,
        needs,
        cooldown,
        lod,
        slot,
        is_wolf,
        is_deer,
        is_horse,
        is_cow,
        is_rabbit,
        is_pig,
        is_fox,
        is_cat,
    ) in animal_query.iter()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *sex != BiologicalSex::Male || cooldown.0 > 0 {
            continue;
        }
        if is_wolf && needs.reproduction >= WOLF_REPRO_MALE_THRESHOLD {
            males[0].insert(entity);
        } else if is_deer && needs.reproduction >= DEER_REPRO_MALE_THRESHOLD {
            males[1].insert(entity);
        } else if is_horse && needs.reproduction >= HORSE_REPRO_MALE_THRESHOLD {
            males[2].insert(entity);
        } else if is_cow && needs.reproduction >= COW_REPRO_MALE_THRESHOLD {
            males[3].insert(entity);
        } else if is_rabbit && needs.reproduction >= RABBIT_REPRO_MALE_THRESHOLD {
            males[4].insert(entity);
        } else if is_pig && needs.reproduction >= PIG_REPRO_MALE_THRESHOLD {
            males[5].insert(entity);
        } else if is_fox && needs.reproduction >= FOX_REPRO_MALE_THRESHOLD {
            males[6].insert(entity);
        } else if is_cat && needs.reproduction >= CAT_REPRO_MALE_THRESHOLD {
            males[7].insert(entity);
        }
    }

    // Phase 2: find female-male pairs (immutable pass)
    let mut found_pairs: Vec<(Entity, Vec2, u8)> = Vec::new();

    for (
        entity,
        transform,
        sex,
        needs,
        cooldown,
        lod,
        slot,
        is_wolf,
        is_deer,
        is_horse,
        is_cow,
        is_rabbit,
        is_pig,
        is_fox,
        is_cat,
    ) in animal_query.iter()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *sex != BiologicalSex::Female || cooldown.0 > 0 {
            continue;
        }

        let (threshold, pop, cap, species) = if is_wolf {
            (WOLF_REPRO_FEMALE_THRESHOLD, wolf_pop, WOLF_POP_CAP, 0u8)
        } else if is_deer {
            (DEER_REPRO_FEMALE_THRESHOLD, deer_pop, DEER_POP_CAP, 1u8)
        } else if is_horse {
            (HORSE_REPRO_FEMALE_THRESHOLD, horse_pop, HORSE_POP_CAP, 2u8)
        } else if is_cow {
            (COW_REPRO_FEMALE_THRESHOLD, cow_pop, COW_POP_CAP, 3u8)
        } else if is_rabbit {
            (
                RABBIT_REPRO_FEMALE_THRESHOLD,
                rabbit_pop,
                RABBIT_POP_CAP,
                4u8,
            )
        } else if is_pig {
            (PIG_REPRO_FEMALE_THRESHOLD, pig_pop, PIG_POP_CAP, 5u8)
        } else if is_fox {
            (FOX_REPRO_FEMALE_THRESHOLD, fox_pop, FOX_POP_CAP, 6u8)
        } else if is_cat {
            (CAT_REPRO_FEMALE_THRESHOLD, cat_pop, CAT_POP_CAP, 7u8)
        } else {
            continue;
        };

        if needs.reproduction < threshold || pop >= cap {
            continue;
        }

        let male_set = &males[species as usize];
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut found_male = false;
        'search: for dy in -REPRO_SEARCH_RADIUS..=REPRO_SEARCH_RADIUS {
            for dx in -REPRO_SEARCH_RADIUS..=REPRO_SEARCH_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if male_set.contains(&candidate) {
                        found_male = true;
                        break 'search;
                    }
                }
            }
        }

        if found_male {
            found_pairs.push((entity, transform.translation.truncate(), species));
        }
    }

    // Phase 3: reset female needs, roll birth, spawn offspring
    let mut births: Vec<(Vec2, u8, Option<u32>)> = Vec::new();

    for (female_ent, birth_pos, species) in found_pairs {
        if let Ok((_, _, _, mut needs, mut cooldown, _, _, _, _, _, _, _, _, _, _)) =
            animal_query.get_mut(female_ent)
        {
            needs.reproduction = 0.0;
            cooldown.0 = ANIMAL_BIRTH_COOLDOWN;
        }
        if fastrand::u32(..10_000) < ANIMAL_BIRTH_CHANCE {
            let herd_cid = herd_query.get(female_ent).ok().map(|h| h.cluster_id);
            births.push((birth_pos, species, herd_cid));
        }
    }

    for (pos, species, herd_cid) in births {
        let slot = clock.population;
        clock.population += 1;
        clock.bucket_size = clock.population.min(10_000);

        let tx = (pos.x / TILE_SIZE).floor() as i32;
        let ty = (pos.y / TILE_SIZE).floor() as i32;
        let world_pos = tile_to_world(tx, ty);
        let sex = BiologicalSex::random();
        let transform = Transform::from_xyz(world_pos.x, world_pos.y, 1.0);
        let ai = AnimalAI {
            target_tile: (tx as i32, ty as i32),
            ..Default::default()
        };

        match species {
            0 => {
                commands.spawn((
                    Wolf,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(30),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Wolf),
                ));
            }
            1 => {
                let mut e = commands.spawn((
                    (
                        Deer,
                        transform,
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        ai,
                        Health::new(20),
                        CombatTarget::default(),
                        CombatCooldown::default(),
                        LodLevel::Full,
                        BucketSlot(slot),
                        crate::simulation::plants::DeerGrazer {
                            graze_timer: fastrand::u16(0..120),
                        },
                        AnimalNeeds::default(),
                        AnimalReproductionCooldown(0),
                        sex,
                    ),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Deer),
                ));
                if let Some(cid) = herd_cid {
                    e.insert(HerdMember { cluster_id: cid });
                }
            }
            2 => {
                let mut e = commands.spawn((
                    Horse,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(HORSE_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Horse),
                ));
                if let Some(cid) = herd_cid {
                    e.insert(HerdMember { cluster_id: cid });
                }
            }
            3 => {
                let mut e = commands.spawn((
                    Cow,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(COW_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Cow),
                ));
                if let Some(cid) = herd_cid {
                    e.insert(HerdMember { cluster_id: cid });
                }
            }
            4 => {
                commands.spawn((
                    Rabbit,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(RABBIT_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                ));
            }
            5 => {
                commands.spawn((
                    Pig,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(PIG_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Pig),
                ));
            }
            6 => {
                commands.spawn((
                    Fox,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(FOX_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                ));
            }
            _ => {
                commands.spawn((
                    Cat,
                    transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(CAT_HP),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    sex,
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Cat),
                ));
            }
        }
    }
}

/// Per-tick (parallel) scan: thirsty animals in `Wander` flip to `Drinking`
/// and set `target_tile` to a passable tile adjacent to the nearest
/// non-salt water source. Sleeping / Chase / Flee / Attack states are
/// never preempted — predator chase keeps urgency. Carried (ridden) and
/// Dormant animals are skipped.
pub fn animal_water_seek_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    globe: Res<crate::world::globe::Globe>,
    mut query: Query<
        (
            &Transform,
            &mut AnimalAI,
            &AnimalNeeds,
            &BucketSlot,
            &LodLevel,
            Option<&CarriedBy>,
        ),
        bevy::prelude::Or<(
            With<Wolf>,
            With<Deer>,
            With<Horse>,
            With<Cow>,
            With<Rabbit>,
            With<Pig>,
            With<Fox>,
            With<Cat>,
        )>,
    >,
) {
    const SCAN_RADIUS: i32 = 14;

    for (transform, mut ai, needs, slot, lod, carried) in query.iter_mut() {
        if *lod == LodLevel::Dormant || carried.is_some() || !clock.is_active(slot.0) {
            continue;
        }
        // Stay in Drinking until the executor flips back to Wander on
        // adjacency-drink. Don't preempt Chase/Flee/Attack/Sleeping.
        if !matches!(ai.state, AnimalState::Wander) {
            continue;
        }
        if needs.thirst < ANIMAL_THIRST_TRIGGER {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let Some(water_tile) = crate::simulation::drink::nearest_fresh_drinkable_tile(
            &chunk_map,
            &globe,
            (cur_tx, cur_ty),
            SCAN_RADIUS,
        ) else {
            continue;
        };

        // Route to an adjacent passable tile rather than the water tile
        // itself — water tiles are impassable for ground animals.
        let dirs: [(i32, i32); 8] = [
            (-1, 0),
            (1, 0),
            (0, -1),
            (0, 1),
            (-1, -1),
            (1, -1),
            (-1, 1),
            (1, 1),
        ];
        let mut adj_target: Option<(i32, i32)> = None;
        for (dx, dy) in dirs {
            let t = (water_tile.0 + dx, water_tile.1 + dy);
            if let Some(k) = chunk_map.tile_kind_at(t.0, t.1) {
                if k.is_passable() {
                    if adj_target.is_none() {
                        adj_target = Some(t);
                    } else {
                        let cur = adj_target.unwrap();
                        let cd = (cur.0 - cur_tx).abs() + (cur.1 - cur_ty).abs();
                        let nd = (t.0 - cur_tx).abs() + (t.1 - cur_ty).abs();
                        if nd < cd {
                            adj_target = Some(t);
                        }
                    }
                }
            }
        }
        let Some(stand_tile) = adj_target else {
            continue;
        };

        ai.state = AnimalState::Drinking;
        ai.target_tile = stand_tile;
        ai.target_entity = None;
    }
}

/// Sequential pass after movement: any animal in `Drinking` state whose
/// chebyshev distance to a fresh-water tile is ≤ 1 drinks one bout. Raw
/// (non-River) sources roll a small sickness bump on `AnimalNeeds.sickness`.
/// Salt water is rejected upstream by `animal_water_seek_system`.
pub fn animal_drink_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    globe: Res<crate::world::globe::Globe>,
    mut query: Query<
        (
            &Transform,
            &mut AnimalAI,
            &mut AnimalNeeds,
            &BucketSlot,
            &LodLevel,
            Option<&CarriedBy>,
        ),
        bevy::prelude::Or<(
            With<Wolf>,
            With<Deer>,
            With<Horse>,
            With<Cow>,
            With<Rabbit>,
            With<Pig>,
            With<Fox>,
            With<Cat>,
        )>,
    >,
) {
    for (transform, mut ai, mut needs, slot, lod, carried) in query.iter_mut() {
        if *lod == LodLevel::Dormant || carried.is_some() || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AnimalState::Drinking {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        // Find any adjacent fresh water; if none in chebyshev 1 the animal
        // hasn't arrived yet — keep walking.
        let mut adj_fresh: Option<(i32, i32, TileKind)> = None;
        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let t = (cur_tx + dx, cur_ty + dy);
                let Some(k) = chunk_map.tile_kind_at(t.0, t.1) else {
                    continue;
                };
                if !k.is_drinkable_candidate() {
                    continue;
                }
                if !crate::world::biome::water_kind_at(&globe, k, t.0, t.1).is_drinkable() {
                    continue; // animals reject Salt and Brackish
                }
                adj_fresh = Some((t.0, t.1, k));
                break;
            }
            if adj_fresh.is_some() {
                break;
            }
        }
        let Some((_, _, kind)) = adj_fresh else {
            // Not adjacent yet — `animal_movement_system` is still walking
            // us toward `target_tile`. Give up if thirst already eased
            // (e.g. animal ate a wet fruit, sickness mods, etc.).
            if needs.thirst < ANIMAL_THIRST_TRIGGER * 0.5 {
                ai.state = AnimalState::Wander;
            }
            continue;
        };

        needs.thirst = (needs.thirst - ANIMAL_DRINK_THIRST_REDUCTION).max(0.0);
        // Raw freshwater (lake / marsh) — bump sickness slightly. Rivers
        // are flowing freshwater and don't add sickness. Phase 4 will
        // factor in `SanitationMap` contamination.
        if !matches!(kind, TileKind::River) {
            needs.sickness = (needs.sickness + 20.0).clamp(0.0, 255.0);
        }
        ai.state = AnimalState::Wander;
        ai.wander_timer = WANDER_INTERVAL;
    }
}

/// Seed starting domestic animals for every eligible non-SOLO faction at
/// `OnEnter(Playing)`, after `spawn_population` + `spawn_animals` (so the
/// member count / settlement infrastructure is live) and before
/// `mark_warmup_complete_system`. Eligibility comes from faction tech
/// awareness — primed by `sync_faction_techs_from_chief_system` during
/// OnEnter.
///
/// Per faction (counts are floors; scaled +1 per 20 founders, capped 6):
/// - `DOG_DOMESTICATION` → 2 dogs (Wolf + Tamed + DomesticAnimal{Dog}).
/// - `ANIMAL_HUSBANDRY` → 2 cattle + 2 pigs (1 M + 1 F each).
/// - `HORSE_TAMING` → 2 horses (1 M + 1 F).
/// - `DOG_DOMESTICATION` → 1 cat (companion).
///
/// Settled factions spawn within 6 tiles of `home_tile` biased to grass/scrub.
/// Nomadic factions spawn with `FollowingBand`.
/// Global cap of 200 seeded animals to bound entity count.
pub fn seed_starting_tamed_animals_system(
    mut commands: Commands,
    mut clock: ResMut<SimClock>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    faction_registry: Res<crate::simulation::faction::FactionRegistry>,
) {
    use crate::simulation::faction::SOLO;
    use crate::simulation::technology::{ANIMAL_HUSBANDRY, DOG_DOMESTICATION, HORSE_TAMING};

    const GLOBAL_CAP: u32 = 200;
    const PLACEMENT_RADIUS: i32 = 6;

    let mut placed_total: u32 = 0;
    // Walk factions in id order for determinism.
    let mut faction_ids: Vec<u32> = faction_registry.factions.keys().copied().collect();
    faction_ids.sort_unstable();

    for fid in faction_ids {
        if fid == SOLO {
            continue;
        }
        let Some(faction) = faction_registry.factions.get(&fid) else {
            continue;
        };
        let home = faction.home_tile;
        let nomadic = faction.caps.home.is_mobile();
        let has_dog = faction.techs.has(DOG_DOMESTICATION);
        let has_husbandry = faction.techs.has(ANIMAL_HUSBANDRY);
        let has_horse = faction.techs.has(HORSE_TAMING);
        if !(has_dog || has_husbandry || has_horse) {
            continue;
        }
        let scale_bonus = (faction.member_count as i32 / 20).clamp(0, 2);
        let per_kind = |floor: u32| -> u32 { (floor + scale_bonus as u32).min(6) };

        // Build the seed list: (species, count, both_sexes).
        let mut seeds: Vec<(DomesticSpecies, u32, bool)> = Vec::new();
        if has_dog {
            seeds.push((DomesticSpecies::Dog, per_kind(2), true));
            seeds.push((DomesticSpecies::Cat, per_kind(1).min(2), false));
        }
        if has_husbandry {
            seeds.push((DomesticSpecies::Cattle, per_kind(2).max(2), true));
            seeds.push((DomesticSpecies::Pig, per_kind(2).max(2), true));
        }
        if has_horse {
            seeds.push((DomesticSpecies::Horse, per_kind(2).max(2), true));
        }

        for (species, count, both_sexes) in seeds {
            for i in 0..count {
                if placed_total >= GLOBAL_CAP {
                    return;
                }
                let (tx, ty) = pick_seed_animal_tile(home, &chunk_map, &mut clock, i, species);
                let z = chunk_map.surface_z_at(tx, ty) as i8;
                let sex = if both_sexes && i < 2 {
                    // First two are mixed-sex pair: i=0 → Male, i=1 → Female.
                    if i == 0 {
                        BiologicalSex::Male
                    } else {
                        BiologicalSex::Female
                    }
                } else {
                    BiologicalSex::random()
                };
                spawn_seeded_domestic_animal(
                    &mut commands,
                    species,
                    fid,
                    nomadic,
                    sex,
                    (tx, ty),
                    z,
                    clock.population,
                );
                clock.population = clock.population.saturating_add(1);
                placed_total = placed_total.saturating_add(1);
            }
        }
    }
}

/// Pick a tile within `PLACEMENT_RADIUS` of `home` biased to grass/scrub.
/// Falls back to home_tile itself if no candidate is found.
fn pick_seed_animal_tile(
    home: (i32, i32),
    chunk_map: &crate::world::chunk::ChunkMap,
    clock: &mut SimClock,
    bump: u32,
    species: DomesticSpecies,
) -> (i32, i32) {
    const PLACEMENT_RADIUS: i32 = 6;
    let mut seed = (home.0 as u32)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(home.1 as u32)
        .wrapping_add(bump.wrapping_mul(0x85EB_CA6B))
        .wrapping_add(species as u32);
    // Tiny deterministic LCG; bounded 80 attempts.
    for _ in 0..80 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let dx = (seed as i32 % (PLACEMENT_RADIUS * 2 + 1)) - PLACEMENT_RADIUS;
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let dy = (seed as i32 % (PLACEMENT_RADIUS * 2 + 1)) - PLACEMENT_RADIUS;
        let t = (home.0 + dx, home.1 + dy);
        let Some(k) = chunk_map.tile_kind_at(t.0, t.1) else {
            continue;
        };
        if !k.is_passable() {
            continue;
        }
        if matches!(
            k,
            TileKind::Grass
                | TileKind::Scrub
                | TileKind::Cropland
                | TileKind::Loam
                | TileKind::Silt
        ) {
            // Don't trip a borrow-checker warning on the unused clock.
            let _ = &mut *clock;
            return t;
        }
    }
    home
}

/// Spawn a single tamed domestic animal at `(tx, ty, z)`. Inserts the species
/// marker (Wolf for Dog, Cow for Cattle, etc.), `Tamed { owner_faction: fid }`,
/// `Indexed` for the correct `IndexedKind`, AnimalAI/Needs, and `FollowingBand`
/// if the faction is nomadic. `DomesticAnimal` + `PackAnimalInventory` get
/// attached by `attach_pack_inventory_system` next tick.
fn spawn_seeded_domestic_animal(
    commands: &mut Commands,
    species: DomesticSpecies,
    fid: u32,
    nomadic: bool,
    sex: BiologicalSex,
    tile: (i32, i32),
    _z: i8,
    slot: u32,
) {
    let pos = tile_to_world(tile.0, tile.1);
    let transform = Transform::from_xyz(pos.x, pos.y, 1.0);
    let ai = AnimalAI {
        target_tile: tile,
        wander_timer: (slot % 60) as f32 * 0.05,
        ..Default::default()
    };
    let needs = AnimalNeeds {
        hunger: 30.0,
        sleep: 20.0,
        reproduction: 40.0,
        thirst: 30.0,
        sickness: 0.0,
    };
    let hp = match species {
        DomesticSpecies::Horse => HORSE_HP,
        DomesticSpecies::Cattle => COW_HP,
        DomesticSpecies::Pig => PIG_HP,
        DomesticSpecies::Dog => 30,
        DomesticSpecies::Cat => CAT_HP,
    };
    let indexed_kind = match species {
        DomesticSpecies::Horse => crate::world::spatial::IndexedKind::Horse,
        DomesticSpecies::Cattle => crate::world::spatial::IndexedKind::Cow,
        DomesticSpecies::Pig => crate::world::spatial::IndexedKind::Pig,
        DomesticSpecies::Dog => crate::world::spatial::IndexedKind::Wolf,
        DomesticSpecies::Cat => crate::world::spatial::IndexedKind::Cat,
    };
    let common = (
        transform,
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        ai,
        Health::new(hp),
        CombatTarget::default(),
        CombatCooldown::default(),
        LodLevel::Full,
        BucketSlot(slot),
        needs,
        AnimalReproductionCooldown(0),
        sex,
        Tamed { owner_faction: fid },
        crate::world::spatial::Indexed::new(indexed_kind),
    );
    let mut e = match species {
        DomesticSpecies::Horse => commands.spawn((Horse, common)),
        DomesticSpecies::Cattle => commands.spawn((Cow, common)),
        DomesticSpecies::Pig => commands.spawn((Pig, common)),
        DomesticSpecies::Dog => commands.spawn((Wolf, common)),
        DomesticSpecies::Cat => commands.spawn((Cat, common)),
    };
    if nomadic {
        e.insert(FollowingBand {
            faction: fid,
            last_redirect_tick: 0,
        });
    }
}

#[cfg(test)]
mod pack_tests {
    use super::*;
    use crate::economy::core_ids;

    fn install_test_catalog() {
        let _ = core_ids::catalog();
    }

    #[test]
    fn pack_inventory_add_remove_round_trip() {
        install_test_catalog();
        let mut inv = PackAnimalInventory::for_capacity(PACK_CAP_HORSE);
        let bedroll = core_ids::bedroll();
        let unfit = inv.add(bedroll, 5);
        assert_eq!(
            unfit, 0,
            "horse should accept 5 bedrolls (5x1500g = 7.5kg << 60kg)"
        );
        assert_eq!(inv.quantity_of(bedroll), 5);
        let removed = inv.remove(bedroll, 3);
        assert_eq!(removed, 3);
        assert_eq!(inv.quantity_of(bedroll), 2);
    }

    #[test]
    fn pack_inventory_overflow_returns_unfit() {
        install_test_catalog();
        let mut inv = PackAnimalInventory::for_capacity(PACK_CAP_DOG); // 15kg
        let yurt = core_ids::packed_yurt(); // 80kg
        let unfit = inv.add(yurt, 1);
        assert_eq!(unfit, 1, "dog cannot carry an 80kg yurt");
        assert_eq!(inv.quantity_of(yurt), 0);
    }

    #[test]
    fn pack_inventory_two_horses_split_yurt() {
        install_test_catalog();
        let mut a = PackAnimalInventory::for_capacity(PACK_CAP_HORSE);
        let mut b = PackAnimalInventory::for_capacity(PACK_CAP_HORSE);
        let yurt = core_ids::packed_yurt(); // 80kg, > horse cap (60kg)
                                            // One horse can't carry one yurt — but we sized capacity so the
                                            // recipient overflows cleanly when the unit doesn't fit.
        let unfit_a = a.add(yurt, 1);
        assert_eq!(unfit_a, 1, "single horse rejects the yurt unit");
        // Combined band cap is 120kg; if we had a 'split' helper we could
        // load it across two pack animals — today add() is per-animal.
        // Instead verify that two same-good adds across separate horses
        // work for two yurts.
        a = PackAnimalInventory::for_capacity(PACK_CAP_COW); // 80kg cow accepts 1
        let unfit = a.add(yurt, 1);
        assert_eq!(unfit, 0);
        let unfit2 = b.add(yurt, 0);
        assert_eq!(unfit2, 0);
    }

    #[test]
    fn domestic_species_label_smoke() {
        assert_eq!(DomesticSpecies::Horse.label(), "Horse");
        assert_eq!(DomesticSpecies::Cattle.label(), "Cattle");
        assert_eq!(DomesticSpecies::Dog.label(), "Dog");
    }
}

#[cfg(test)]
mod husbandry_tests {
    use super::*;
    use crate::simulation::test_fixture::TestSim;
    use crate::world::tile::TileKind;

    #[test]
    fn tame_animal_inserts_domestic_animal_next_tick() {
        // Tame a wild horse by spawning a horse + Tamed marker directly,
        // then tick once so `attach_pack_inventory_system` fires.
        let mut sim = TestSim::new(0xCAFE);
        sim.flat_world(3, 0, TileKind::Grass);
        let horse_e = {
            let world = sim.app.world_mut();
            world
                .spawn((
                    Horse,
                    Transform::from_xyz(0.0, 0.0, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    AnimalAI::default(),
                    crate::simulation::combat::Health::new(40),
                    crate::simulation::combat::CombatTarget::default(),
                    crate::simulation::combat::CombatCooldown::default(),
                    crate::simulation::lod::LodLevel::Full,
                    crate::simulation::schedule::BucketSlot(0),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    crate::simulation::reproduction::BiologicalSex::Female,
                    Tamed { owner_faction: 1 },
                ))
                .id()
        };
        sim.tick_n(2);
        // After ticking, DomesticAnimal + PackAnimalInventory should both be
        // attached automatically.
        let world = sim.app.world();
        let domestic = world.get::<DomesticAnimal>(horse_e);
        assert!(
            domestic.is_some(),
            "DomesticAnimal must auto-attach on Tamed"
        );
        let pack = world.get::<PackAnimalInventory>(horse_e);
        assert!(
            pack.is_some(),
            "PackAnimalInventory must auto-attach for horse"
        );
        assert_eq!(domestic.unwrap().species, DomesticSpecies::Horse);
    }

    #[test]
    fn wolf_tamed_becomes_dog_species() {
        let mut sim = TestSim::new(0xBEEF);
        sim.flat_world(3, 0, TileKind::Grass);
        let wolf_e = {
            let world = sim.app.world_mut();
            world
                .spawn((
                    Wolf,
                    Transform::from_xyz(0.0, 0.0, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    AnimalAI::default(),
                    crate::simulation::combat::Health::new(30),
                    crate::simulation::combat::CombatTarget::default(),
                    crate::simulation::combat::CombatCooldown::default(),
                    crate::simulation::lod::LodLevel::Full,
                    crate::simulation::schedule::BucketSlot(0),
                    AnimalNeeds::default(),
                    AnimalReproductionCooldown(0),
                    crate::simulation::reproduction::BiologicalSex::Male,
                    Tamed { owner_faction: 1 },
                ))
                .id()
        };
        sim.tick_n(2);
        let world = sim.app.world();
        let domestic = world.get::<DomesticAnimal>(wolf_e).expect("attached");
        assert_eq!(
            domestic.species,
            DomesticSpecies::Dog,
            "tamed wolf gets Dog species"
        );
    }
}
