//! Fishing — terrain-based renewable food (unlocked by the Mesolithic
//! `FISHING` tech).
//!
//! Fish are modeled as local renewable **stocks** (`FishStock`), never as
//! per-tile entities. A stock is *lazily* initialized the first time a water
//! tile is fished, deterministically from the world seed — untouched water
//! reads as full capacity, so the map stays sparse (only depleted/touched
//! tiles carry an entry). Daily logistic regeneration runs in the Economy
//! schedule. Habitat (River / Lake / Marsh / Coast) is **derived** from
//! `TileKind` + salinity at lookup time, never stored as terrain.
//!
//! Spot discovery is a terrain scan (`nearest_fishable_spot`) — water is
//! static terrain, so there is no `SharedKnowledge` cluster bookkeeping.
//! Claims reuse `GatherClaims` keyed on `(spot_tile, Resource(fish))`.

use ahash::AHashMap;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::economy::core_ids;
use crate::economy::item::Item;
use crate::game_state::WorldSeed;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::carry::Carrier;
use crate::simulation::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use crate::simulation::gather_claims::{release_gather_claim, GatherClaims};
use crate::simulation::htn::{record_routing_failure, record_target_failure, MethodHistory};
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::MemoryKind;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::ActivityKind;
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::biome::{water_kind_at, WaterKind};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::globe::Globe;
use crate::world::seasons::{Calendar, Season, TICKS_PER_DAY};
use crate::world::terrain::{tile_to_world, world_to_tile};
use crate::world::tile::TileKind;

// ── Tunables ──────────────────────────────────────────────────────────────────

/// Work ticks to land one catch. `Trap` is cheaper labour than an actively
/// worked `Handline` but lands a smaller haul (see `base_catch`).
pub const FISH_WORK_TICKS_HANDLINE: u8 = 90;
pub const FISH_WORK_TICKS_TRAP: u8 = 55;

/// Per-habitat base biomass capacity (≈ fish a full stock holds). Coastal
/// shoals are richest, marshes leanest.
const CAP_RIVER: f32 = 9.0;
const CAP_LAKE: f32 = 13.0;
const CAP_MARSH: f32 = 6.0;
const CAP_COAST: f32 = 16.0;

/// Daily logistic regen rate (fraction of the logistic term per game-day).
const REGEN_RATE: f32 = 0.35;
/// Flat recruitment floor so a fully depleted stock still recovers (a pure
/// `biomass × rate` logistic term is zero at `biomass == 0`).
const REGEN_SEED: f32 = 0.6;
/// A stock at/above this fraction of capacity is "recovered" — its entry is
/// dropped from the sparse map so untouched water stays implicit-full.
const RECOVERED_FRACTION: f32 = 0.995;

/// Terrain-scan radius for the HTN method precondition + dispatcher.
pub const FISHING_SEARCH_RADIUS: i32 = 14;

// ── Habitat ───────────────────────────────────────────────────────────────────

/// Where a stock lives. Derived from `TileKind` + salinity by [`habitat_at`];
/// never persisted as terrain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FishHabitat {
    River,
    Lake,
    Marsh,
    Coast,
}

impl FishHabitat {
    fn base_capacity(self) -> f32 {
        match self {
            FishHabitat::River => CAP_RIVER,
            FishHabitat::Lake => CAP_LAKE,
            FishHabitat::Marsh => CAP_MARSH,
            FishHabitat::Coast => CAP_COAST,
        }
    }

    /// Per-completion yield multiplier — richer habitats land more per cast.
    fn yield_mult(self) -> f32 {
        match self {
            FishHabitat::River => 1.0,
            FishHabitat::Lake => 1.1,
            FishHabitat::Marsh => 0.8,
            FishHabitat::Coast => 1.25,
        }
    }
}

/// Derive the fishable habitat of a tile, or `None` if it is not fishable.
/// `River` / `Marsh` are always fresh; open `Water` is `Coast` when salt or
/// brackish, `Lake` when fresh. `Bridge` is an access tile (handled by the
/// routing layer), `Dam` blocks the water — neither is a fishing spot.
pub fn habitat_at(chunk_map: &ChunkMap, globe: &Globe, tile: (i32, i32)) -> Option<FishHabitat> {
    match chunk_map.tile_kind_at(tile.0, tile.1)? {
        TileKind::River => Some(FishHabitat::River),
        TileKind::Marsh => Some(FishHabitat::Marsh),
        TileKind::Water => match water_kind_at(globe, TileKind::Water, tile.0, tile.1) {
            WaterKind::Salt | WaterKind::Brackish => Some(FishHabitat::Coast),
            WaterKind::Fresh => Some(FishHabitat::Lake),
        },
        _ => None,
    }
}

// ── Fishing method ────────────────────────────────────────────────────────────

/// How a catch is taken. v1 ships `Handline`; `Trap` is wired but unused by
/// the HTN method. `Weir` / `Net` / `BoatLine` are reserved for later
/// tech/structure upgrades (data-model only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Trap/Weir/Net/BoatLine are reserved for later upgrades.
pub enum FishingMethod {
    Handline,
    Trap,
    Weir,
    Net,
    BoatLine,
}

impl FishingMethod {
    pub fn work_ticks(self) -> u8 {
        match self {
            FishingMethod::Trap => FISH_WORK_TICKS_TRAP,
            _ => FISH_WORK_TICKS_HANDLINE,
        }
    }

    /// Base catch in biomass units before habitat / season / skill scaling.
    fn base_catch(self) -> f32 {
        match self {
            FishingMethod::Handline => 2.0,
            FishingMethod::Trap => 1.5,
            FishingMethod::Net => 3.0,
            FishingMethod::Weir => 4.0,
            FishingMethod::BoatLine => 3.0,
        }
    }

    fn skill_xp(self) -> u32 {
        match self {
            FishingMethod::Trap => 2,
            _ => 3,
        }
    }
}

// ── Seasonality ───────────────────────────────────────────────────────────────

/// Seasonal yield multiplier — spring/autumn river runs lift river/marsh
/// catches; winter depresses open water everywhere.
fn season_yield_mult(habitat: FishHabitat, season: Season) -> f32 {
    match (habitat, season) {
        (_, Season::Winter) => 0.55,
        (FishHabitat::River | FishHabitat::Marsh, Season::Spring | Season::Autumn) => 1.35,
        (_, Season::Summer) => 1.0,
        _ => 1.1,
    }
}

/// Seasonal regen multiplier — same shape as the yield curve so depleted
/// spring/autumn rivers also bounce back faster.
fn season_regen_mult(habitat: FishHabitat, season: Season) -> f32 {
    match (habitat, season) {
        (_, Season::Winter) => 0.4,
        (FishHabitat::River | FishHabitat::Marsh, Season::Spring | Season::Autumn) => 1.4,
        _ => 1.0,
    }
}

// ── FishStock ─────────────────────────────────────────────────────────────────

/// One water tile's live fish stock. Absent tiles are implicitly full.
#[derive(Clone, Copy, Debug)]
pub struct FishStockCell {
    pub habitat: FishHabitat,
    pub biomass: f32,
    pub capacity: f32,
}

/// Sparse tile-keyed renewable fish stocks. Lives across chunk streaming
/// (simulation state, not terrain — never restamped on chunk load). Only
/// tiles that have actually been fished carry an entry; everything else is
/// implicitly at capacity.
#[derive(Resource, Default)]
pub struct FishStock {
    pub by_tile: AHashMap<(i32, i32), FishStockCell>,
}

/// Deterministic per-tile hash — same pattern as `chunk_streaming`'s plant
/// scatter. Drives lazy capacity init so a tile reads the same stock every
/// run for a given world seed.
fn tile_hash(seed: u64, tile: (i32, i32)) -> u64 {
    let mut h = seed ^ 0x9E37_79B9_7F4A_7C15;
    h ^= (tile.0 as i64 as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
    h = h.rotate_left(31);
    h ^= (tile.1 as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
}

/// Map a hash to `[0, 1)`.
fn hash_frac(h: u64) -> f32 {
    ((h >> 40) & 0xFF_FFFF) as f32 / ((1u64 << 24) as f32)
}

/// Deterministic capacity for a fresh stock at `tile`: the habitat base
/// scaled `0.75..1.25` by the seeded tile hash. Riverside abundance gets a
/// small lift from `river_distance_at` (channels near other water are richer).
fn fresh_capacity(habitat: FishHabitat, seed: u64, tile: (i32, i32), chunk_map: &ChunkMap) -> f32 {
    let jitter = 0.75 + 0.5 * hash_frac(tile_hash(seed, tile));
    let river_lift = if chunk_map.river_distance_at(tile.0, tile.1) <= 3 {
        1.15
    } else {
        1.0
    };
    (habitat.base_capacity() * jitter * river_lift).max(1.0)
}

impl FishStock {
    /// Read the live biomass at `tile`. Untouched tiles read as full
    /// capacity, so callers see a sensible value without forcing an entry.
    pub fn biomass_at(
        &self,
        habitat: FishHabitat,
        seed: u64,
        tile: (i32, i32),
        chunk_map: &ChunkMap,
    ) -> f32 {
        match self.by_tile.get(&tile) {
            Some(cell) => cell.biomass,
            None => fresh_capacity(habitat, seed, tile, chunk_map),
        }
    }

    /// Borrow (lazily creating) the stock cell for `tile`. The cell is
    /// materialised at full capacity on first touch.
    pub fn get_or_init(
        &mut self,
        habitat: FishHabitat,
        seed: u64,
        tile: (i32, i32),
        chunk_map: &ChunkMap,
    ) -> &mut FishStockCell {
        self.by_tile.entry(tile).or_insert_with(|| {
            let capacity = fresh_capacity(habitat, seed, tile, chunk_map);
            FishStockCell {
                habitat,
                biomass: capacity,
                capacity,
            }
        })
    }

    /// Remove `amount` biomass from `tile`, clamped to `0..=capacity`.
    /// Returns the amount actually taken.
    pub fn harvest(
        &mut self,
        habitat: FishHabitat,
        seed: u64,
        tile: (i32, i32),
        chunk_map: &ChunkMap,
        amount: f32,
    ) -> f32 {
        let cell = self.get_or_init(habitat, seed, tile, chunk_map);
        let taken = amount.clamp(0.0, cell.biomass);
        cell.biomass = (cell.biomass - taken).clamp(0.0, cell.capacity);
        taken
    }
}

// ── Daily regeneration ────────────────────────────────────────────────────────

/// Daily logistic regrowth of every depleted stock. Runs in `Economy`. Walks
/// only the sparse populated entries; cells that recover to capacity are
/// dropped so the map stays small.
pub fn fish_regen_system(
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    mut stock: ResMut<FishStock>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let season = calendar.season;
    stock.by_tile.retain(|_, cell| {
        let regen_mul = season_regen_mult(cell.habitat, season);
        let logistic = (1.0 - cell.biomass / cell.capacity).max(0.0);
        let grow = (cell.biomass + REGEN_SEED) * REGEN_RATE * logistic * regen_mul;
        cell.biomass = (cell.biomass + grow).clamp(0.0, cell.capacity);
        // Drop fully-recovered cells — an absent tile reads as implicit-full.
        cell.biomass < cell.capacity * RECOVERED_FRACTION
    });
}

// ── Terrain scan ──────────────────────────────────────────────────────────────

/// True when `tile` has at least one passable land tile chebyshev-adjacent to
/// it — i.e. a worker could stand somewhere to fish it.
fn has_stand_tile(chunk_map: &ChunkMap, tile: (i32, i32)) -> bool {
    for dy in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let (nx, ny) = (tile.0 + dx, tile.1 + dy);
            let nz = chunk_map.surface_z_at(nx, ny);
            if chunk_map.passable_at(nx, ny, nz) {
                return true;
            }
        }
    }
    false
}

/// Nearest fishable water tile (`River`/`Marsh`/`Water`, with a passable
/// adjacent stand tile) within `radius` of `from`. **`ChunkMap`-only** — this
/// is the HTN method precondition scan, snapshot into `PlannerCtx.fish_spot_tile`
/// by the `AcquireFood` / `StockpileFood` dispatchers.
///
/// Deliberately does *not* consult `FishStock`: depleted-spot avoidance is the
/// registry's job — when the executor finds an exhausted stock it records a
/// `MethodHistory` failure, `score_method_with_history` biases the fishing
/// method down, and forage wins until daily regen refills the fishery. Water
/// is static terrain, so there are no memory clusters; the routing layer
/// (`assign_task_with_routing`) resolves the actual stand tile + reachability.
pub fn nearest_fishable_water(
    chunk_map: &ChunkMap,
    from: (i32, i32),
    radius: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<((i32, i32), i32)> = None;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tile = (from.0 + dx, from.1 + dy);
            let fishable = matches!(
                chunk_map.tile_kind_at(tile.0, tile.1),
                Some(TileKind::River | TileKind::Marsh | TileKind::Water)
            );
            if !fishable || !has_stand_tile(chunk_map, tile) {
                continue;
            }
            let dist = dx.abs().max(dy.abs());
            match best {
                None => best = Some((tile, dist)),
                Some((_, d)) if dist < d => best = Some((tile, dist)),
                _ => {}
            }
        }
    }
    best.map(|(t, _)| t)
}

// ── Executor ──────────────────────────────────────────────────────────────────

/// Routing resources for `fish_task_system`, bundled to stay under Bevy's
/// per-system param ceiling.
#[derive(SystemParam)]
pub struct FishRouting<'w, 's> {
    pub storage_tile_map: Res<'w, StorageTileMap>,
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub gather_claims: Res<'w, GatherClaims>,
    _marker: std::marker::PhantomData<&'s ()>,
}

/// Spill `qty` of `resource_id` at `(tx, ty)` as a `GroundItem`.
fn spill_ground(commands: &mut Commands, tx: i32, ty: i32, item: Item, qty: u32) {
    if qty == 0 {
        return;
    }
    let pos = tile_to_world(tx, ty);
    commands.spawn((
        GroundItem { item, qty },
        Transform::from_xyz(pos.x, pos.y, 0.3),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
    ));
}

/// Chain handoff after a `Task::Fish` finishes: release the claim, advance
/// the prefetch ring, then route the trailing leg (`Eat` in place, or
/// `DepositToFactionStorage` to faction storage). Mirrors `gather::finish_gather`.
fn finish_fish(
    ai: &mut PersonAI,
    aq: &mut ActionQueue,
    actor: Entity,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    faction_id: Option<u32>,
    chunk_map: &ChunkMap,
    routing: &FishRouting,
    method_history: &mut MethodHistory,
    now: u64,
    completed: bool,
) {
    release_gather_claim(&routing.gather_claims, ai, actor);
    ai.state = AiState::Idle;
    ai.target_entity = None;
    ai.work_progress = 0;

    if !completed {
        // The queued tail (Eat / Deposit) was predicated on a catch. Record a
        // `MethodHistory` failure so `score_method_with_history` biases the
        // fishing method down — a depleted/invalidated spot then loses to
        // forage until `fish_regen_system` refills the stock.
        record_target_failure(method_history, ai, now);
        aq.cancel();
        return;
    }
    aq.advance();

    match aq.current {
        Task::Eat => {
            // Survive chain: eat the catch in place.
            ai.state = AiState::Working;
        }
        Task::DepositToFactionStorage {
            target_faction_id, ..
        } => {
            let Some(fid) = target_faction_id.or(faction_id) else {
                record_routing_failure(method_history, ai, now);
                aq.cancel();
                return;
            };
            let Some(storage_tile) = routing.storage_tile_map.nearest_for_faction(fid, cur_tile)
            else {
                record_routing_failure(method_history, ai, now);
                aq.cancel();
                return;
            };
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                storage_tile,
                TaskKind::DepositResource,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                chunk_map,
                &routing.chunk_connectivity,
            );
            if !dispatched {
                record_routing_failure(method_history, ai, now);
                aq.cancel();
            }
        }
        _ => {}
    }
}

/// Fishing executor. Sequential, before `gather::gather_system`. On arrival
/// (the worker is `Working` chebyshev-adjacent to the water spot), accumulates
/// `FishingMethod::work_ticks`, harvests the tile's `FishStock`, and deposits
/// the catch into a free hand (overflow spills as a `GroundItem`).
#[allow(clippy::too_many_arguments)]
pub fn fish_task_system(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    globe: Res<Globe>,
    seed: Res<WorldSeed>,
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    mut stock: ResMut<FishStock>,
    mut faction_registry: ResMut<FactionRegistry>,
    routing: FishRouting,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut ActionQueue,
        &mut Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        &Transform,
        Option<&FactionMember>,
        &mut MethodHistory,
    )>,
) {
    let fish_id = core_ids::fish();
    for (
        actor,
        mut ai,
        mut aq,
        mut carrier,
        mut skills,
        slot,
        lod,
        transform,
        faction_member,
        mut method_history,
    ) in agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working || aq.current_task_kind() != TaskKind::Fishing as u16 {
            continue;
        }
        let Some((spot_tile, method, _out)) = aq.current.as_fish() else {
            // Typed-channel desync — defence in depth.
            aq.cancel_chain(&mut ai);
            continue;
        };

        let faction_id = faction_member
            .map(|fm| fm.faction_id)
            .filter(|&id| id != SOLO);
        let cur_tile = world_to_tile(transform.translation.truncate());
        let cur_chunk = ChunkCoord(
            cur_tile.0.div_euclid(CHUNK_SIZE as i32),
            cur_tile.1.div_euclid(CHUNK_SIZE as i32),
        );

        // Revalidate: tile still fishable, worker still adjacent.
        let habitat = habitat_at(&chunk_map, &globe, spot_tile);
        let adjacent = (cur_tile.0 - spot_tile.0).abs().max((cur_tile.1 - spot_tile.1).abs()) <= 1;
        let Some(habitat) = habitat.filter(|_| adjacent) else {
            finish_fish(
                &mut ai,
                &mut aq,
                actor,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
                &mut method_history,
                clock.tick,
                false,
            );
            continue;
        };

        // Accumulate work.
        if ai.work_progress < method.work_ticks() {
            continue;
        }
        ai.work_progress = 0;

        let available = stock.biomass_at(habitat, seed.0, spot_tile, &chunk_map);
        if available <= 0.0 {
            finish_fish(
                &mut ai,
                &mut aq,
                actor,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
                &mut method_history,
                clock.tick,
                false,
            );
            continue;
        }

        // Faction food-yield multiplier (folds in the FISHING tech bonus) +
        // activity-log credit for tech discovery.
        let food_mul = if let Some(fid) = faction_id {
            if let Some(fd) = faction_registry.factions.get_mut(&fid) {
                fd.activity_log.increment(ActivityKind::Fishing);
                fd.food_yield_multiplier()
            } else {
                1.0
            }
        } else {
            1.0
        };

        let skill = skills.get(SkillKind::Fishing);
        let skill_factor = 0.8 + (skill as f32 / 255.0) * 0.6;
        let raw = method.base_catch()
            * habitat.yield_mult()
            * season_yield_mult(habitat, calendar.season)
            * skill_factor
            * food_mul;
        let want = raw.min(available);
        let taken = stock.harvest(habitat, seed.0, spot_tile, &chunk_map, want);
        let qty = (taken.round() as u32).max(1);

        // Deposit the catch: free hand first, overflow spills at the worker's
        // tile as a `GroundItem`.
        let item = Item::new_commodity(fish_id);
        let leftover = carrier.try_pick_up(item, qty);
        if leftover > 0 {
            spill_ground(&mut commands, cur_tile.0, cur_tile.1, item, leftover);
        }

        skills.gain_xp(SkillKind::Fishing, method.skill_xp());

        finish_fish(
            &mut ai,
            &mut aq,
            actor,
            cur_tile,
            cur_chunk,
            faction_id,
            &chunk_map,
            &routing,
            &mut method_history,
            clock.tick,
            true,
        );
    }
}

/// `MemoryKind` used to key fishing's `GatherClaims` entries.
pub fn fish_claim_kind() -> MemoryKind {
    MemoryKind::Resource(core_ids::fish())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_capacity_is_deterministic_per_seed_and_tile() {
        let map = ChunkMap::default();
        let a = fresh_capacity(FishHabitat::River, 42, (10, 20), &map);
        let b = fresh_capacity(FishHabitat::River, 42, (10, 20), &map);
        let c = fresh_capacity(FishHabitat::River, 99, (10, 20), &map);
        assert_eq!(a, b, "same seed+tile must yield the same capacity");
        assert!((a - c).abs() > f32::EPSILON, "a different seed shifts capacity");
        assert!(a >= 1.0);
    }

    #[test]
    fn untouched_tile_reads_full_capacity() {
        let map = ChunkMap::default();
        let stock = FishStock::default();
        let cap = fresh_capacity(FishHabitat::Lake, 7, (3, 3), &map);
        let live = stock.biomass_at(FishHabitat::Lake, 7, (3, 3), &map);
        assert_eq!(cap, live, "an absent tile must read implicit-full");
    }

    #[test]
    fn harvest_clamps_to_zero_and_capacity() {
        let map = ChunkMap::default();
        let mut stock = FishStock::default();
        let cap = fresh_capacity(FishHabitat::Marsh, 1, (0, 0), &map);
        // Over-harvest never drives biomass negative.
        let taken = stock.harvest(FishHabitat::Marsh, 1, (0, 0), &map, cap * 10.0);
        assert!((taken - cap).abs() < 0.001, "can only take what is there");
        assert_eq!(stock.by_tile[&(0, 0)].biomass, 0.0);
    }

    #[test]
    fn regen_never_overshoots_capacity_or_goes_negative() {
        let map = ChunkMap::default();
        let mut stock = FishStock::default();
        // Deplete a cell, then run many days of regen.
        stock.harvest(FishHabitat::River, 5, (2, 2), &map, 999.0);
        let cap = stock.by_tile[&(2, 2)].capacity;
        for _ in 0..200 {
            if let Some(cell) = stock.by_tile.get_mut(&(2, 2)) {
                let logistic = (1.0 - cell.biomass / cell.capacity).max(0.0);
                let grow = (cell.biomass + REGEN_SEED) * REGEN_RATE * logistic;
                cell.biomass = (cell.biomass + grow).clamp(0.0, cell.capacity);
                assert!(cell.biomass >= 0.0 && cell.biomass <= cap + 0.001);
            }
        }
        // A depleted river must actually recover (the REGEN_SEED floor).
        assert!(stock.by_tile[&(2, 2)].biomass > cap * 0.5);
    }

    #[test]
    fn season_winter_depresses_yield() {
        let summer = season_yield_mult(FishHabitat::Lake, Season::Summer);
        let winter = season_yield_mult(FishHabitat::Lake, Season::Winter);
        assert!(winter < summer);
    }

    #[test]
    fn method_work_ticks_and_catch_order() {
        assert!(FishingMethod::Trap.work_ticks() < FishingMethod::Handline.work_ticks());
        assert!(FishingMethod::Trap.base_catch() < FishingMethod::Handline.base_catch());
    }
}
