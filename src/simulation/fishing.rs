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

/// Minimum size of a connected `River`/`Marsh`/`Water` component for a tile to
/// count as fishable. Wells (1 cell), tiny dug pits, and dam-pinched pools fall
/// below this; rivers, lakes, marshes, and real impoundments easily clear it.
pub const MIN_FISHABLE_WATER_TILES: usize = 8;

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
/// routing layer), `Dam` blocks the water — neither is a fishing spot. The
/// tile must also belong to a connected water component of at least
/// `MIN_FISHABLE_WATER_TILES` cells (see [`tile_supports_fishery`]) — dug
/// wells, tiny pits, and pinched dam pools fail by size.
pub fn habitat_at(chunk_map: &ChunkMap, globe: &Globe, tile: (i32, i32)) -> Option<FishHabitat> {
    if !tile_supports_fishery(chunk_map, tile) {
        return None;
    }
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

/// True when `tile` is a `River`/`Marsh`/`Water` cell whose 4-connected
/// water component contains at least `MIN_FISHABLE_WATER_TILES` cells.
/// `Bridge` (decking; water flows beneath) and `Dam` are not walked through —
/// banks on either side of a bridge clear the threshold on their own. BFS
/// early-outs once `MIN_FISHABLE_WATER_TILES` distinct cells have been visited.
fn tile_supports_fishery(chunk_map: &ChunkMap, tile: (i32, i32)) -> bool {
    let is_water = |t: (i32, i32)| {
        matches!(
            chunk_map.tile_kind_at(t.0, t.1),
            Some(TileKind::River | TileKind::Marsh | TileKind::Water)
        )
    };
    if !is_water(tile) {
        return false;
    }
    let mut visited: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::default();
    let mut frontier: Vec<(i32, i32)> = Vec::with_capacity(MIN_FISHABLE_WATER_TILES * 2);
    visited.insert(tile);
    frontier.push(tile);
    if visited.len() >= MIN_FISHABLE_WATER_TILES {
        return true;
    }
    while let Some((x, y)) = frontier.pop() {
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let n = (x + dx, y + dy);
            if visited.contains(&n) {
                continue;
            }
            if !is_water(n) {
                continue;
            }
            visited.insert(n);
            if visited.len() >= MIN_FISHABLE_WATER_TILES {
                return true;
            }
            frontier.push(n);
        }
    }
    false
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
            if !tile_supports_fishery(chunk_map, tile) || !has_stand_tile(chunk_map, tile) {
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
    pub spatial_index: Res<'w, crate::world::spatial::SpatialIndex>,
    pub stand_reservations:
        Res<'w, crate::simulation::stand_reservation::StandTileReservations>,
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
        GroundItem {
            item,
            qty,
            owner_household: None,
        },
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
    ai.target_entity = None;

    if !completed {
        // The queued tail (Eat / Deposit) was predicated on a catch. Record a
        // `MethodHistory` failure so `score_method_with_history` biases the
        // fishing method down — a depleted/invalidated spot then loses to
        // forage until `fish_regen_system` refills the stock.
        record_target_failure(method_history, ai, now);
        aq.cancel_chain(ai);
        return;
    }
    aq.finish_task(ai);

    match aq.current {
        Task::Eat => {
            // Survive chain: eat the catch in place.
            aq.begin_working(ai);
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
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                chunk_map,
                &routing.chunk_connectivity,
                &routing.spatial_index,
                &routing.stand_reservations,
                actor,
                now,);
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
        Option<&crate::simulation::tools::ToolKit>,
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
        toolkit,
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

        // Realistic Tool Overhaul: fishing requires a Fishing Kit. No kit ⇒
        // failed outcome. A worker with NO `ToolKit` component at all (fixture
        // agents) degrades gracefully — treated as equipped.
        {
            use crate::simulation::tools::{ToolRequirement, ToolUseKind};
            let kit_req = ToolRequirement::any(ToolUseKind::Fish);
            let has_kit = toolkit.map(|tk| tk.satisfies(&kit_req)).unwrap_or(true);
            if !has_kit {
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
        }

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
    use crate::world::chunk::{Chunk, ChunkCoord, CHUNK_SIZE};
    use crate::world::tile::TileData;

    /// Build a single-chunk `ChunkMap` whose every surface tile reads as
    /// `base_kind` at z=0. Caller patches in water tiles via `set_tile`.
    fn chunk_map_filled(base_kind: TileKind) -> ChunkMap {
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[base_kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        let chunk = Chunk::new(surface_z, surface_kind, surface_fertility);
        let mut map = ChunkMap::default();
        map.0.insert(ChunkCoord(0, 0), chunk);
        map
    }

    fn paint(map: &mut ChunkMap, tile: (i32, i32), kind: TileKind) {
        map.set_tile(
            tile.0,
            tile.1,
            0,
            TileData {
                kind,
                ..Default::default()
            },
        );
    }

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

    #[test]
    fn single_water_tile_is_not_fishable() {
        // A lone Water cell (e.g. a dug well shaft) in a sea of Grass must
        // not register as a fishing spot — the component is one tile.
        let mut map = chunk_map_filled(TileKind::Grass);
        paint(&mut map, (5, 5), TileKind::Water);
        assert!(!tile_supports_fishery(&map, (5, 5)));
        assert!(nearest_fishable_water(&map, (5, 5), 8).is_none());
        let globe = crate::world::globe::Globe::new(0);
        assert!(habitat_at(&map, &globe, (5, 5)).is_none());
    }

    #[test]
    fn large_lake_is_fishable() {
        // 4×4 Water block (16 cells, well above MIN_FISHABLE_WATER_TILES=8).
        let mut map = chunk_map_filled(TileKind::Grass);
        for dy in 0..4 {
            for dx in 0..4 {
                paint(&mut map, (10 + dx, 10 + dy), TileKind::Water);
            }
        }
        assert!(tile_supports_fishery(&map, (11, 11)));
        // Stand tile available (Grass neighbours), spot returned.
        assert!(nearest_fishable_water(&map, (10, 10), 8).is_some());
    }

    #[test]
    fn river_strip_is_fishable() {
        // A 10×1 River line — long enough to clear the size threshold.
        let mut map = chunk_map_filled(TileKind::Grass);
        for dx in 0..10 {
            paint(&mut map, (4 + dx, 7), TileKind::River);
        }
        assert!(tile_supports_fishery(&map, (8, 7)));
        let globe = crate::world::globe::Globe::new(0);
        assert!(matches!(
            habitat_at(&map, &globe, (8, 7)),
            Some(FishHabitat::River)
        ));
    }

    #[test]
    fn well_shaft_pattern_is_not_fishable() {
        // Replicate the 5×5 well projection: a single central Water cell
        // (the shaft), with the surrounding ring of Wall lining + carved
        // Dirt floor. No connected water → not fishable.
        let mut map = chunk_map_filled(TileKind::Grass);
        // Wall lining ring (5×5 outer rim, excluding the gateway gap).
        for dy in 0..5 {
            for dx in 0..5 {
                let on_rim = dx == 0 || dx == 4 || dy == 0 || dy == 4;
                if !on_rim {
                    continue;
                }
                // Leave one tile open as the gateway, like a real well.
                if dx == 2 && dy == 0 {
                    continue;
                }
                paint(&mut map, (20 + dx, 20 + dy), TileKind::Wall);
            }
        }
        // Carved Dirt floors inside the ring.
        for dy in 1..4 {
            for dx in 1..4 {
                paint(&mut map, (20 + dx, 20 + dy), TileKind::Dirt);
            }
        }
        // Single water-shaft cell at the centre.
        paint(&mut map, (22, 22), TileKind::Water);
        assert!(!tile_supports_fishery(&map, (22, 22)));
        assert!(nearest_fishable_water(&map, (22, 22), 8).is_none());
    }
}
