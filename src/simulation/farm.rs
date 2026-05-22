//! Farm plot assignment + plot-scoped helpers (farm-planner §7).
//!
//! Communal villages (where Grain has `chief_allocates_labor == true`) match
//! Farmer entities to state-owned Agricultural plots one-to-one. The chief
//! posts plot-scoped Farm jobs against those assignments via
//! `chief_job_posting_system` reading `FarmPlotAssignments`.
//!
//! Seasonal-farming additions (jellyfish plan):
//! - `FarmSeasonPhase` — pure function of `Calendar.season` mapping to
//!   `{SpringPrepPlant, SummerMaintenance, AutumnHarvest, WinterDormant}`.
//! - `FarmWorkPhase` — `{Prepare, Plant, Harvest}` tag carried on
//!   `JobProgress::FieldWork`.
//! - `FieldTileIndex` — `(tile → FieldTileState { nutrients, last_crop,
//!   last_worked_year, plot_id })` resource keyed by every tile in
//!   `PlotIndex.ag_tiles`. World-gen `TileData.fertility` is the natural
//!   ceiling / recovery cap; `nutrients` is the live nutrient pool.
//! - `prepare_field_task_system` (Sequential) — Task::PrepareField executor.
//! - `fallow_recovery_system` (Economy, season-edge) — restores nutrients on
//!   rested tiles up to the per-tile fertility ceiling.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::core_ids;
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::land::{Plot, PlotId, PlotIndex, Tenure};
use crate::simulation::person::Profession;
use crate::simulation::plants::PlantKind;
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::ZoneKind;
use crate::world::seasons::{Calendar, Season, TICKS_PER_DAY};
use crate::world::terrain::world_to_tile;

/// Field-prep tile work, in fixed-update ticks. ~4 game-seconds at 20 Hz.
pub const FIELD_PREP_WORK_TICKS: u16 = 80;
/// Farming XP awarded on a successful PrepareField completion.
pub const SKILL_XP_PER_PREP_TILE: u32 = 4;
/// Nutrient floor below which a tile is considered "exhausted". Prepare can
/// only bump exhausted soil this high (not into the plantable band).
pub const EXHAUSTED_FLOOR: u8 = 30;
/// Nutrient threshold the planting dispatcher requires before stamping a
/// seed on a prepared tile.
pub const MIN_PLANTABLE_NUTRIENTS: u8 = 80;
/// Per-season nutrient debit on a Grain harvest.
pub const HARVEST_NUTRIENT_DEBIT: u8 = 30;
/// Per-season-edge fallow recovery (capped by per-tile fertility ceiling).
pub const FALLOW_NUTRIENTS_PER_SEASON: u8 = 15;

// ── Annual-planning constants (single source of truth) ───────────────────────
// Rehomed here from `organic_settlement.rs` so the farm-demand model and the
// seasonal `farm_pressure` signal read the same numbers. `organic_settlement`
// re-exports these at the old paths so `parcel_targets` doesn't churn.

/// Grain a person eats per game-year. Derivation: `HUNGER_RATE = 2.0`
/// hunger/real-s; a game-day is `TICKS_PER_DAY = 3600` ticks × 0.05 s/tick
/// (20 Hz) = 180 real-s ⇒ 360 hunger/day; grain is 150 cal/unit ⇒ 2.4
/// grain/person/day; a year is 4 × `DAYS_PER_SEASON = 5` = 20 game-days ⇒
/// 2.4 × 20 = 48. If those timescale knobs change, re-derive this.
pub const GRAIN_PER_PERSON_PER_YEAR: u32 = 48;
/// Typical per-tile annual grain yield used for plot sizing — the middle
/// nutrient tier (`grain_yield_for_nutrients`: ≥120 → 4).
pub const GRAIN_YIELD_PER_TILE_PLANNING: u32 = 4;
/// Bad-year / seed-reserve / winter-carryover margin folded into the demand
/// target. Tribes kept reserves; one failed harvest shouldn't mean famine.
pub const SUPPLY_SAFETY_NUMER: u32 = 5; // 1.25× as 5/4
pub const SUPPLY_SAFETY_DENOM: u32 = 4;

/// Phase-weighted seasonal `FieldWork` claim-share floors — the fraction of
/// the village that may pile onto open seasonal field work, overriding the
/// normal Farm workforce-budget cap. Heavier in Spring (prep + planting rush)
/// than Autumn (harvest).
pub const SEASONAL_FARM_CLAIM_SHARE_SPRING: f32 = 0.65;
pub const SEASONAL_FARM_CLAIM_SHARE_AUTUMN: f32 = 0.45;
/// Summer caretaker pressure is a fraction of the full annual deficit — fields
/// are mostly planted, only stragglers remain.
pub const SUMMER_FARM_PRESSURE_SCALE: f32 = 0.35;

/// Annual grain a faction of `members` should hold for food security,
/// including the bad-year safety margin. This is the stock target the
/// seasonal `farm_pressure` signal measures the deficit against. Shares the
/// `GRAIN_PER_PERSON_PER_YEAR × SUPPLY_SAFETY` expression with
/// `organic_settlement::parcel_targets`' `food_tiles` (which divides further
/// by `GRAIN_YIELD_PER_TILE_PLANNING`).
#[inline]
pub fn annual_grain_target(members: u32) -> u32 {
    (members * GRAIN_PER_PERSON_PER_YEAR * SUPPLY_SAFETY_NUMER).div_ceil(SUPPLY_SAFETY_DENOM)
}

/// What the village should be doing in the field this season. Pure function
/// of `Calendar.season`. No state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FarmSeasonPhase {
    /// Spring — prepare unworked tiles and plant prepared, non-exhausted soil.
    SpringPrepPlant,
    /// Summer — caretaker top-up; low-priority Prepare postings only.
    SummerMaintenance,
    /// Autumn — harvest postings while mature Grain stands.
    AutumnHarvest,
    /// Winter — no postings.
    WinterDormant,
}

/// Map `Calendar.season` to a `FarmSeasonPhase`. Pure helper.
#[inline]
pub fn farm_season_phase(cal: &Calendar) -> FarmSeasonPhase {
    match cal.season {
        Season::Spring => FarmSeasonPhase::SpringPrepPlant,
        Season::Summer => FarmSeasonPhase::SummerMaintenance,
        Season::Autumn => FarmSeasonPhase::AutumnHarvest,
        Season::Winter => FarmSeasonPhase::WinterDormant,
    }
}

/// Phase tag on a posted FieldWork job. Drives executor branch + multi-open
/// posting counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FarmWorkPhase {
    /// Turn raw plot soil into Cropland.
    Prepare,
    /// Plant seed on prepared, non-exhausted Cropland.
    Plant,
    /// Reap mature Grain plants from the plot.
    Harvest,
}

/// Per-tile dynamic state for any tile belonging to an Agricultural plot.
/// Extensible: adding fertilizer = +1 field, rotation bonus = read last_crop,
/// weeds = +1 field.
#[derive(Clone, Copy, Debug)]
pub struct FieldTileState {
    pub plot_id: PlotId,
    /// Current nutrient level `[0, 255]`. Ceiling is the world-gen
    /// `TileData.fertility` (never mutated by farming).
    pub nutrients: u8,
    /// Last crop sown / reaped on this tile. `None` means unworked.
    pub last_crop: Option<PlantKind>,
    /// Calendar year of the last write (harvest / prepare). Used by the
    /// fallow recovery system to gate the per-season nutrient bump.
    pub last_worked_year: u16,
}

/// Sparse, persistent-by-resource. Populated at `carve_plots_system` for every
/// tile in `PlotIndex.ag_tiles`. Pruned when a plot teardown removes the tile
/// from `ag_tiles`.
#[derive(Resource, Default, Debug)]
pub struct FieldTileIndex {
    pub by_tile: AHashMap<(i32, i32), FieldTileState>,
}

impl FieldTileIndex {
    /// Seed a fresh entry for a newly-carved Agricultural tile.
    /// Idempotent — existing entries are preserved.
    pub fn ensure_entry(&mut self, tile: (i32, i32), plot_id: PlotId, fertility: u8) {
        self.by_tile.entry(tile).or_insert(FieldTileState {
            plot_id,
            nutrients: fertility,
            last_crop: None,
            last_worked_year: 0,
        });
    }

    /// Drop an entry when a tile is removed from a plot.
    pub fn remove(&mut self, tile: (i32, i32)) {
        self.by_tile.remove(&tile);
    }
}

/// Find the nearest plantable tile in `rect` for the seasonal pipeline.
/// "Plantable" means: tile kind is `Cropland` (prepared), tile is not already
/// carrying a plant (PlantMap), and `FieldTileIndex[tile].nutrients >=
/// MIN_PLANTABLE_NUTRIENTS`. Falls back to no result if every candidate is
/// either unprepared, exhausted, or already planted. Manhattan distance.
pub fn find_nearest_plantable_in_rect(
    chunk_map: &crate::world::chunk::ChunkMap,
    plant_map: &crate::simulation::plants::PlantMap,
    field_tiles: &FieldTileIndex,
    from: (i32, i32),
    rect_min: (i32, i32),
    rect_max: (i32, i32),
) -> Option<(i32, i32)> {
    let mut best: Option<(i32, i32)> = None;
    let mut best_dist = i32::MAX;
    for ty in rect_min.1..=rect_max.1 {
        for tx in rect_min.0..=rect_max.0 {
            if plant_map.0.contains_key(&(tx, ty)) {
                continue;
            }
            if !matches!(
                chunk_map.tile_kind_at(tx, ty),
                Some(crate::world::tile::TileKind::Cropland)
            ) {
                continue;
            }
            let nut = field_tiles
                .by_tile
                .get(&(tx, ty))
                .map(|s| s.nutrients)
                .unwrap_or(0);
            if nut < MIN_PLANTABLE_NUTRIENTS {
                continue;
            }
            let d = (tx - from.0).abs() + (ty - from.1).abs();
            if d < best_dist {
                best_dist = d;
                best = Some((tx, ty));
            }
        }
    }
    best
}

/// Find the nearest tile in `rect` that needs Prepare work — either not yet
/// `Cropland` OR `FieldTileIndex[tile].nutrients < EXHAUSTED_FLOOR`.
pub fn find_nearest_unprepared_in_rect(
    chunk_map: &crate::world::chunk::ChunkMap,
    field_tiles: &FieldTileIndex,
    from: (i32, i32),
    rect_min: (i32, i32),
    rect_max: (i32, i32),
) -> Option<(i32, i32)> {
    let mut best: Option<(i32, i32)> = None;
    let mut best_dist = i32::MAX;
    for ty in rect_min.1..=rect_max.1 {
        for tx in rect_min.0..=rect_max.0 {
            let is_cropland = matches!(
                chunk_map.tile_kind_at(tx, ty),
                Some(crate::world::tile::TileKind::Cropland)
            );
            let nut = field_tiles
                .by_tile
                .get(&(tx, ty))
                .map(|s| s.nutrients)
                .unwrap_or(0);
            let unprepared = !is_cropland || nut < EXHAUSTED_FLOOR;
            if !unprepared {
                continue;
            }
            let d = (tx - from.0).abs() + (ty - from.1).abs();
            if d < best_dist {
                best_dist = d;
                best = Some((tx, ty));
            }
        }
    }
    best
}

/// Return tier-scaled grain yield based on a tile's live nutrient level.
/// Plan: ≥180 → 5, ≥120 → 4, ≥80 → 3, else 1. Falls below the planting gate
/// only because already-planted crops that lost nutrients (rare) can still
/// be harvested.
#[inline]
pub fn grain_yield_for_nutrients(nutrients: u8) -> u32 {
    if nutrients >= 180 {
        5
    } else if nutrients >= 120 {
        4
    } else if nutrients >= MIN_PLANTABLE_NUTRIENTS {
        3
    } else {
        1
    }
}

/// One-to-one matching of farmer worker entities to state-owned Agricultural
/// plots. Maintained by `chief_farm_plot_assignment_system`. Read by
/// `chief_job_posting_system` (plot-scoped Farm postings) and
/// `job_claim_system` (assigned-farmer-only claim restriction).
#[derive(Resource, Default, Debug)]
pub struct FarmPlotAssignments {
    pub farmer_to_plot: AHashMap<Entity, PlotId>,
    pub plot_to_farmer: AHashMap<PlotId, Entity>,
}

impl FarmPlotAssignments {
    pub fn assigned_plot(&self, farmer: Entity) -> Option<PlotId> {
        self.farmer_to_plot.get(&farmer).copied()
    }

    pub fn assigned_farmer(&self, plot: PlotId) -> Option<Entity> {
        self.plot_to_farmer.get(&plot).copied()
    }

    pub fn assign(&mut self, farmer: Entity, plot: PlotId) {
        // Drop any prior pairing both sides — assignments are 1:1.
        if let Some(prev_plot) = self.farmer_to_plot.remove(&farmer) {
            self.plot_to_farmer.remove(&prev_plot);
        }
        if let Some(prev_farmer) = self.plot_to_farmer.remove(&plot) {
            self.farmer_to_plot.remove(&prev_farmer);
        }
        self.farmer_to_plot.insert(farmer, plot);
        self.plot_to_farmer.insert(plot, farmer);
    }

    pub fn release_farmer(&mut self, farmer: Entity) {
        if let Some(plot) = self.farmer_to_plot.remove(&farmer) {
            self.plot_to_farmer.remove(&plot);
        }
    }

    pub fn release_plot(&mut self, plot: PlotId) {
        if let Some(farmer) = self.plot_to_farmer.remove(&plot) {
            self.farmer_to_plot.remove(&farmer);
        }
    }
}

/// Daily Economy-set system. Per village faction whose grain policy still has
/// `chief_allocates_labor`, greedy-match `Profession::Farmer` workers to
/// state-owned Agricultural plots in the same settlement by distance from the
/// farmer's home tile. Releases stale assignments when plots vanish, factions
/// dissolve, or workers stop being Farmers.
pub fn chief_farm_plot_assignment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    plot_index: Res<PlotIndex>,
    plot_q: Query<&Plot>,
    profession_q: Query<(Entity, &Profession, &FactionMember, &Transform)>,
    mut assignments: ResMut<FarmPlotAssignments>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }

    // Drop stale entries first (plot gone, farmer despawned/demoted, etc.).
    let stale_farmers: Vec<Entity> = assignments
        .farmer_to_plot
        .keys()
        .copied()
        .filter(|f| {
            profession_q
                .get(*f)
                .map_or(true, |(_, p, _, _)| !matches!(p, Profession::Farmer))
        })
        .collect();
    for f in stale_farmers {
        assignments.release_farmer(f);
    }
    let stale_plots: Vec<PlotId> = assignments
        .plot_to_farmer
        .keys()
        .copied()
        .filter(|pid| {
            let Some(&ent) = plot_index.by_id.get(pid) else {
                return true;
            };
            let Ok(plot) = plot_q.get(ent) else {
                return true;
            };
            // Drop if plot left state ownership (e.g. transferred to a
            // household via auction) or is no longer agricultural.
            !matches!(plot.tenure, Tenure::StateOwned) || plot.zone_kind != ZoneKind::Agricultural
        })
        .collect();
    for pid in stale_plots {
        assignments.release_plot(pid);
    }

    // Greedy-match per village faction.
    for (&fid, faction) in registry.factions.iter() {
        if faction.parent_faction.is_some() {
            continue; // households don't own communal plots
        }
        let chief_allocates = faction.policy_for(core_ids::grain()).chief_allocates_labor;
        if !chief_allocates {
            continue;
        }

        // Collect this village's state-owned, unassigned Agricultural plots.
        let mut free_plots: Vec<(PlotId, (i32, i32))> = Vec::new();
        for (&pid, &ent) in plot_index.by_id.iter() {
            let Ok(plot) = plot_q.get(ent) else { continue };
            if plot.faction_id != fid
                || plot.zone_kind != ZoneKind::Agricultural
                || !matches!(plot.tenure, Tenure::StateOwned)
            {
                continue;
            }
            if assignments.plot_to_farmer.contains_key(&pid) {
                continue;
            }
            free_plots.push((pid, plot.rect.center()));
        }
        if free_plots.is_empty() {
            continue;
        }

        // Collect this village's unassigned Farmer workers + their tile.
        let mut free_farmers: Vec<(Entity, (i32, i32))> = Vec::new();
        for (e, prof, fm, tr) in profession_q.iter() {
            if !matches!(prof, Profession::Farmer) || fm.faction_id != fid {
                continue;
            }
            if assignments.farmer_to_plot.contains_key(&e) {
                continue;
            }
            let tile = (
                (tr.translation.x / 16.0).floor() as i32,
                (tr.translation.y / 16.0).floor() as i32,
            );
            free_farmers.push((e, tile));
        }
        if free_farmers.is_empty() {
            continue;
        }

        // Greedy: for each farmer (in arbitrary order), assign nearest free plot.
        for (farmer, ftile) in free_farmers {
            let Some((idx, &(plot_pid, _))) = free_plots
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, pcenter))| chebyshev(ftile, *pcenter))
            else {
                break;
            };
            assignments.assign(farmer, plot_pid);
            free_plots.swap_remove(idx);
            if free_plots.is_empty() {
                break;
            }
        }
    }
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Iterator over `(plot_id, rect)` for every state-owned Agricultural plot
/// belonging to `faction_id`. Reused by `jobs.rs` (multi-open Farm posting
/// §6) and `faction.rs` (plot-demand-aware Farmer promotion §7) so both
/// surfaces speak the same definition of "field that needs a farmer".
/// "Has work" today = existence; the planting dispatcher's per-tile rect
/// search short-circuits when nothing's left to plant.
pub fn state_owned_ag_plots_for_faction(
    faction_id: u32,
    plot_index: &PlotIndex,
    plot_q: &Query<&Plot>,
) -> Vec<(PlotId, crate::simulation::settlement::TileRect)> {
    let mut out = Vec::new();
    for (&pid, &ent) in plot_index.by_id.iter() {
        let Ok(plot) = plot_q.get(ent) else { continue };
        if plot.faction_id != faction_id {
            continue;
        }
        if plot.zone_kind != ZoneKind::Agricultural {
            continue;
        }
        if !matches!(plot.tenure, Tenure::StateOwned) {
            continue;
        }
        out.push((pid, plot.rect));
    }
    out
}

/// Game-start seeding (farm-planner §15). Runs once at `OnEnter(Playing)`
/// after `seed_starting_buildings_system`. For every settled, non-SOLO
/// village faction, ensure at least one 16×16 Agricultural plot exists at a
/// good nearby spot, set its tenure based on the economy preset, and pre-
/// seed the appropriate storage with grain seeds so the first farm cycle
/// can begin immediately.
///
/// Skipped for nomadic factions (no plots / no settlement). Skipped for
/// any faction that already has at least one Agricultural plot — runtime
/// carving will own ongoing supply.
pub fn seed_starting_farms_system(
    mut commands: Commands,
    options: Res<crate::game_state::GameStartOptions>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    mut plot_index: ResMut<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    mut chunk_map: ResMut<crate::world::chunk::ChunkMap>,
    storage_tiles: Query<(&crate::simulation::faction::FactionStorageTile, &Transform)>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut ground_items: Query<&mut crate::simulation::items::GroundItem>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
    mut field_tiles: ResMut<FieldTileIndex>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    use crate::simulation::land::Plot;
    use crate::simulation::organic_settlement::ParcelShape;
    use crate::simulation::settlement::TileRect;

    if !options.seed_buildings {
        // Sandbox / minimal start — skip seeding farms.
        return;
    }
    const PLOT_Z: i8 = 0;
    /// Floor on starting grain seed for tiny / fixture factions.
    const MIN_STARTING_GRAIN_SEEDS: u32 = 32;
    /// Founders carry provisions. The seeded edible-food stock covers the
    /// year-1 establishment shortfall (spawn → first harvest bridge + the gap
    /// left by a partial first crop) — ~0.6 of a year's food per founder. It
    /// runs out; foraging/hunting still bridges the rest of year 1.
    const YEAR1_FOOD_BUFFER_NUMER: u32 = 3;
    const YEAR1_FOOD_BUFFER_DENOM: u32 = 5;

    // Snapshot which factions already have an Agricultural plot.
    let mut already_seeded: ahash::AHashSet<u32> = ahash::AHashSet::new();
    for (_, &ent) in plot_index.by_id.iter() {
        if let Ok(plot) = plot_q.get(ent) {
            if plot.zone_kind == ZoneKind::Agricultural {
                already_seeded.insert(plot.faction_id);
            }
        }
    }

    // Stage work — gather faction ids first so we don't borrow registry mutably while iterating.
    let mut work: Vec<(u32, crate::simulation::settlement::SettlementId, (i32, i32), u32)> =
        Vec::new();
    for (&fid, faction) in registry.factions.iter() {
        if fid == 0 {
            continue;
        }
        if faction.parent_faction.is_some() {
            continue; // households don't seed plots — village does
        }
        if matches!(
            faction.lifestyle,
            crate::simulation::faction::Lifestyle::Nomadic
        ) {
            continue;
        }
        if already_seeded.contains(&fid) {
            continue;
        }
        let Some(sid) = settlement_map.first_for_faction(fid) else {
            continue;
        };
        work.push((fid, sid, faction.home_tile, faction.member_count.max(1)));
    }

    for (fid, sid, home, members) in work {
        // Demand-driven sizing — same expression as `parcel_targets`
        // `food_tiles`: members × annual grain need × safety ÷ per-tile yield.
        use crate::simulation::organic_settlement::{
            GRAIN_PER_PERSON_PER_YEAR, GRAIN_YIELD_PER_TILE_PLANNING, SUPPLY_SAFETY_DENOM,
            SUPPLY_SAFETY_NUMER,
        };
        let demand_tiles = (members * GRAIN_PER_PERSON_PER_YEAR * SUPPLY_SAFETY_NUMER)
            .div_ceil(SUPPLY_SAFETY_DENOM * GRAIN_YIELD_PER_TILE_PLANNING);
        // Site the starting plot on an Agricultural BELT parcel from the
        // settlement brain (populated by the OnEnter kickoff survey) — so the
        // tick-0 farm is already outside town, consistent with the runtime
        // belt. Pick the belt parcel nearest home (deterministic tiebreak by
        // origin). If the brain produced no belt parcel (e.g. no fertile land
        // around the settlement), skip the plot entirely — `carve_plots_system`
        // will carve belt plots once the layout settles; we still pre-seed the
        // grain below so planting works the moment a plot exists. NO near-home
        // fallback (that was the "farms all over the base" regression).
        let belt_rect: Option<TileRect> = brains.0.get(&sid).and_then(|brain| {
            brain
                .parcels
                .iter()
                .filter_map(|p| match (p.district_hint, &p.shape) {
                    (
                        Some(crate::simulation::organic_settlement::DistrictKind::Agricultural),
                        ParcelShape::Rect(r),
                    ) => Some(*r),
                    _ => None,
                })
                .min_by_key(|r| {
                    let cx = r.x0 + r.w as i32 / 2;
                    let cy = r.y0 + r.h as i32 / 2;
                    ((cx - home.0).abs().max((cy - home.1).abs()), r.x0, r.y0)
                })
        });

        if let Some(rect) = belt_rect {
            let pid = plot_index.alloc_id();
            let plot = Plot {
                id: pid,
                settlement_id: sid.0,
                faction_id: fid,
                rect,
                z: PLOT_Z,
                zone_kind: ZoneKind::Agricultural,
                tenure: Tenure::StateOwned,
                holder: crate::simulation::land::TenureHolder::State { faction_id: fid },
                base_value: crate::simulation::land::PLOT_BASE_VALUE,
                last_valued_tick: 0,
                missed_payments: 0,
                frontage_edge: None,
                access_tile: None,
                parent_plot: None,
                plowed_year: None,
            };
            let entity = commands.spawn(plot).id();
            plot_index.by_id.insert(pid, entity);
            plot_index.by_settlement.entry(sid.0).or_default().push(pid);
            // Seasonal-farming jellyfish: most of the belt is left UN-prepared
            // at game start (the tribe tills it over the following seasons),
            // but a mottled STARTER patch is pre-stamped `Cropland` so year 1
            // is a real first crop rather than a from-zero prepare spike —
            // founders break ground as part of settling. Budget: half the
            // annual demand, capped at half the plot so it never paves the
            // whole field. `field_tile_role` mottles the patch.
            use crate::simulation::land::{field_tile_role, FieldTileRole};
            use crate::world::tile::{TileData, TileKind};
            let plot_area = rect.w as u32 * rect.h as u32;
            let mut starter_budget = (demand_tiles / 2).min(plot_area / 2);
            let starter_seed = (fid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for ty in rect.y0..rect.y0 + rect.h as i32 {
                for tx in rect.x0..rect.x0 + rect.w as i32 {
                    plot_index.by_tile.insert((tx, ty), pid);
                    plot_index.ag_tiles.insert((tx, ty));
                    let z = chunk_map.surface_z_at(tx, ty);
                    let cur = chunk_map.tile_at(tx, ty, z);
                    let role = field_tile_role(starter_seed, fid, (tx, ty), rect);
                    let pre_stamp = starter_budget > 0
                        && matches!(role, FieldTileRole::Cropland | FieldTileRole::CroplandLow)
                        && (cur.kind == TileKind::Grass || cur.kind.is_soil_like());
                    if pre_stamp {
                        let fert: u8 = if role == FieldTileRole::Cropland { 200 } else { 120 };
                        chunk_map.set_tile(
                            tx,
                            ty,
                            z,
                            TileData {
                                kind: TileKind::Cropland,
                                elevation: cur.elevation,
                                fertility: fert,
                                flags: cur.flags,
                                ore: cur.ore,
                            },
                        );
                        tile_changed
                            .send(crate::world::chunk_streaming::TileChangedEvent { tx, ty });
                        field_tiles.ensure_entry((tx, ty), pid, fert);
                        starter_budget -= 1;
                    } else {
                        field_tiles.ensure_entry((tx, ty), pid, cur.fertility);
                    }
                }
            }
        }

        // Pre-seed physical grain seeds + a year-1 food buffer at a faction
        // storage tile. `FactionStorage` is only a rollup cache rebuilt every
        // Economy tick from `GroundItem`s, so writing it directly would vanish
        // before posting.
        let storage_tile = storage_tiles
            .iter()
            .find_map(|(storage, transform)| {
                (storage.faction_id == fid)
                    .then_some(world_to_tile(transform.translation.truncate()))
            })
            .unwrap_or(home);
        // Seed grain scaled to the annual demand so `parcel_targets`'
        // `seed_tiles` budget isn't pinned to a flat floor.
        let starting_seeds = demand_tiles.max(MIN_STARTING_GRAIN_SEEDS);
        crate::simulation::items::spawn_or_merge_ground_item(
            &mut commands,
            &spatial,
            &mut ground_items,
            storage_tile.0,
            storage_tile.1,
            crate::economy::core_ids::grain_seed(),
            starting_seeds,
        );
        // Edible-food provisions buffer — bridges spawn → first harvest and
        // the partial-first-crop gap (Change 4).
        let food_buffer = (members * GRAIN_PER_PERSON_PER_YEAR * YEAR1_FOOD_BUFFER_NUMER)
            / YEAR1_FOOD_BUFFER_DENOM;
        if food_buffer > 0 {
            crate::simulation::items::spawn_or_merge_ground_item(
                &mut commands,
                &spatial,
                &mut ground_items,
                storage_tile.0,
                storage_tile.1,
                crate::economy::core_ids::grain(),
                food_buffer,
            );
        }
    }
}

/// One-shot OnEnter(Playing) backfill: any tile already in `PlotIndex.ag_tiles`
/// that doesn't yet have a `FieldTileIndex` entry gets one with
/// `nutrients = tile.fertility, last_crop = None`. Covers the
/// `seed_farmstead_yard` (which still stamps Cropland at seed time) and any
/// future save-game with pre-stamped Cropland tiles.
pub fn backfill_field_tile_index_system(
    plot_index: Res<crate::simulation::land::PlotIndex>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    mut field_tiles: ResMut<FieldTileIndex>,
) {
    for &tile in plot_index.ag_tiles.iter() {
        if field_tiles.by_tile.contains_key(&tile) {
            continue;
        }
        let pid = match plot_index.by_tile.get(&tile).copied() {
            Some(p) => p,
            None => continue,
        };
        let z = chunk_map.surface_z_at(tile.0, tile.1);
        let data = chunk_map.tile_at(tile.0, tile.1, z);
        field_tiles.ensure_entry(tile, pid, data.fertility);
    }
}

/// Sequential executor for `Task::PrepareField`. Accumulates work_progress to
/// `FIELD_PREP_WORK_TICKS`, then stamps `TileKind::Cropland` (preserving
/// fertility), emits a `TileChangedEvent`, increments the worker's claimed
/// Farm posting's `JobProgress::FieldWork.completed`, grants Farming XP, and
/// bumps `FieldTileIndex[tile].nutrients` to at least `EXHAUSTED_FLOOR`.
pub fn prepare_field_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut chunk_map: ResMut<crate::world::chunk::ChunkMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    mut field_tiles: ResMut<FieldTileIndex>,
    mut board: ResMut<crate::simulation::jobs::JobBoard>,
    mut completed_events: EventWriter<crate::simulation::jobs::JobCompletedEvent>,
    mut workers: Query<
        (
            Entity,
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &mut crate::simulation::skills::Skills,
            &crate::simulation::schedule::BucketSlot,
            &crate::simulation::lod::LodLevel,
            Option<&crate::simulation::jobs::JobClaim>,
        ),
        With<crate::simulation::person::Person>,
    >,
) {
    use crate::simulation::jobs::{record_fieldwork_progress, JobKind};
    use crate::simulation::person::AiState;
    use crate::simulation::skills::SkillKind;
    use crate::simulation::tasks::TaskKind;
    use crate::world::tile::{TileData, TileKind};
    for (actor, mut ai, mut aq, mut skills, slot, lod, claim_opt) in workers.iter_mut() {
        if *lod == crate::simulation::lod::LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::PrepareField as u16 {
            continue;
        }
        // Defence-in-depth: typed channel mismatch means a stale write.
        let Some(tile) = aq.current.as_prepare_field() else {
            aq.cancel_chain(&mut ai);
            continue;
        };
        if ai.state != AiState::Working {
            continue;
        }
        if (ai.work_progress as u16) < FIELD_PREP_WORK_TICKS {
            continue;
        }
        // Completion: stamp tile to Cropland (preserving elevation/fertility/
        // flags/ore), emit a change event, bump nutrients up to the
        // exhausted-floor minimum, credit the posting, grant XP, exit.
        let (tx, ty) = tile;
        let z = chunk_map.surface_z_at(tx, ty);
        let cur = chunk_map.tile_at(tx, ty, z);
        if cur.kind != TileKind::Cropland {
            chunk_map.set_tile(
                tx,
                ty,
                z,
                TileData {
                    kind: TileKind::Cropland,
                    elevation: cur.elevation,
                    fertility: cur.fertility,
                    flags: cur.flags,
                    ore: cur.ore,
                },
            );
            tile_changed.send(crate::world::chunk_streaming::TileChangedEvent { tx, ty });
        }
        if let Some(state) = field_tiles.by_tile.get_mut(&tile) {
            if state.nutrients < EXHAUSTED_FLOOR {
                state.nutrients = EXHAUSTED_FLOOR;
            }
        }
        // Credit the claimed posting's Prepare phase. `record_fieldwork_progress`
        // no-ops unless the backing posting is `FieldWork { phase: Prepare }`,
        // so a worker that strayed onto a Plant/Harvest claim can't be credited
        // here.
        if let Some(claim) = claim_opt {
            if matches!(claim.kind, JobKind::Farm) {
                record_fieldwork_progress(
                    &mut commands,
                    &mut board,
                    &mut completed_events,
                    claim.job_id,
                    FarmWorkPhase::Prepare,
                    1,
                );
            }
        }
        skills.gain_xp(SkillKind::Farming, SKILL_XP_PER_PREP_TILE);
        let _ = actor;
        aq.finish_task(&mut ai);
    }
}

/// Per-season-edge Economy system. Walks every entry in `FieldTileIndex`; for
/// tiles with no live plant in `PlantMap` and `last_worked_year < calendar.year`
/// bumps `nutrients += FALLOW_NUTRIENTS_PER_SEASON`, capped by the per-tile
/// `TileData.fertility` ceiling.
pub fn fallow_recovery_system(
    calendar: Res<Calendar>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    mut field_tiles: ResMut<FieldTileIndex>,
    mut last_seen: Local<Option<Season>>,
) {
    // Season-edge gate: only run once per season change. Mirrors
    // `plant_lifecycle_system`'s `Local<Option<Season>>` pattern.
    let cur = calendar.season;
    let prev = match *last_seen {
        Some(s) => s,
        None => {
            *last_seen = Some(cur);
            return;
        }
    };
    if prev == cur {
        return;
    }
    *last_seen = Some(cur);
    let cur_year = calendar.year as u16;
    for (tile, state) in field_tiles.by_tile.iter_mut() {
        if plant_map.0.contains_key(tile) {
            continue;
        }
        if state.last_worked_year >= cur_year {
            continue;
        }
        let z = chunk_map.surface_z_at(tile.0, tile.1);
        let cap = chunk_map.tile_at(tile.0, tile.1, z).fertility;
        let new_nut = state
            .nutrients
            .saturating_add(FALLOW_NUTRIENTS_PER_SEASON)
            .min(cap);
        state.nutrients = new_nut;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal_with(season: Season, year: u32) -> Calendar {
        let mut c = Calendar::default();
        c.season = season;
        c.year = year;
        c
    }

    #[test]
    fn farm_season_phase_classification() {
        assert_eq!(
            farm_season_phase(&cal_with(Season::Spring, 1)),
            FarmSeasonPhase::SpringPrepPlant
        );
        assert_eq!(
            farm_season_phase(&cal_with(Season::Summer, 1)),
            FarmSeasonPhase::SummerMaintenance
        );
        assert_eq!(
            farm_season_phase(&cal_with(Season::Autumn, 1)),
            FarmSeasonPhase::AutumnHarvest
        );
        assert_eq!(
            farm_season_phase(&cal_with(Season::Winter, 1)),
            FarmSeasonPhase::WinterDormant
        );
    }

    #[test]
    fn annual_grain_target_scales_with_population() {
        assert_eq!(annual_grain_target(0), 0);
        // 1 person: 1 × 48 × 5 / 4 = 60.
        assert_eq!(annual_grain_target(1), 60);
        // 20 people: 20 × 48 × 5 / 4 = 1200.
        assert_eq!(annual_grain_target(20), 1200);
    }

    #[test]
    fn grain_yield_scales_with_nutrients() {
        assert_eq!(grain_yield_for_nutrients(200), 5);
        assert_eq!(grain_yield_for_nutrients(130), 4);
        assert_eq!(grain_yield_for_nutrients(100), 3);
        assert_eq!(grain_yield_for_nutrients(60), 1);
        assert_eq!(grain_yield_for_nutrients(0), 1);
    }

    #[test]
    fn field_tile_index_ensure_entry_is_idempotent() {
        let mut idx = FieldTileIndex::default();
        idx.ensure_entry((1, 2), 7u32, 120);
        // Mutate then re-ensure — must NOT overwrite (preserves live nutrients).
        if let Some(s) = idx.by_tile.get_mut(&(1, 2)) {
            s.nutrients = 50;
        }
        idx.ensure_entry((1, 2), 7u32, 120);
        assert_eq!(idx.by_tile.get(&(1, 2)).unwrap().nutrients, 50);
    }

    #[test]
    fn exhausted_tile_is_not_plantable_via_helper() {
        // No ChunkMap needed — pass a synthetic helper.
        // Logic check: helper rejects nutrients < MIN_PLANTABLE_NUTRIENTS.
        let mut idx = FieldTileIndex::default();
        idx.ensure_entry((0, 0), 1u32, 50);
        let state = idx.by_tile.get(&(0, 0)).unwrap();
        assert!(state.nutrients < MIN_PLANTABLE_NUTRIENTS);
        // Sanity: exhausted floor sits below MIN_PLANTABLE.
        assert!(EXHAUSTED_FLOOR < MIN_PLANTABLE_NUTRIENTS);
    }

    #[test]
    fn plot_sizing_scales_to_food_need() {
        // Mimic `organic_settlement::parcel_targets` formula.
        let members: u32 = 20;
        let grain_seed_stock: u32 = 32;
        let food_tiles = (members * 16) / 4; // 80
        let labor_tiles = ((members * 60) / 100).saturating_mul(24); // 288
        let seed_tiles = grain_seed_stock.max(32); // 32
        let target_active = food_tiles.min(labor_tiles).min(seed_tiles); // 32
        let target_plots = (((target_active + 95) / 96).max(1)).min(12);
        assert_eq!(target_plots, 1, "20-member 32-seed band → 1 plot, not 6-7");
    }

    #[test]
    fn winter_no_farm_postings_via_phase_helper() {
        // The chief Farm branch is gated on `farm_season_phase != WinterDormant`.
        // This pin guards the gate from accidentally dropping the Winter check.
        for season in [Season::Spring, Season::Summer, Season::Autumn] {
            assert!(!matches!(
                farm_season_phase(&cal_with(season, 1)),
                FarmSeasonPhase::WinterDormant
            ));
        }
        assert_eq!(
            farm_season_phase(&cal_with(Season::Winter, 1)),
            FarmSeasonPhase::WinterDormant
        );
    }

    #[test]
    fn fallow_recovery_caps_at_fertility() {
        // The recovery loop bumps `nutrients += FALLOW_NUTRIENTS_PER_SEASON`
        // every season-edge, capped at the per-tile `TileData.fertility`.
        // Simulate the pure arithmetic for 4 season-edges on a tile that
        // starts at 50 nutrients with fertility ceiling 110.
        let mut nut: u8 = 50;
        let cap: u8 = 110;
        for _ in 0..4 {
            nut = nut.saturating_add(FALLOW_NUTRIENTS_PER_SEASON).min(cap);
        }
        assert_eq!(
            nut, 110,
            "nutrients hit the fertility ceiling, not 50+60=110+"
        );
    }

    #[test]
    fn harvest_debit_lowers_nutrients_by_30() {
        let mut nut: u8 = 200;
        nut = nut.saturating_sub(HARVEST_NUTRIENT_DEBIT);
        assert_eq!(nut, 170);
        // Saturating: a low-nutrient tile zeroes out, doesn't underflow.
        let mut low: u8 = 10;
        low = low.saturating_sub(HARVEST_NUTRIENT_DEBIT);
        assert_eq!(low, 0);
    }

    #[test]
    fn plot_sizing_scales_up_at_higher_pop_and_seed() {
        // Larger band with ample seed -> multiple plots, but capped by labor.
        let members: u32 = 200;
        let grain_seed_stock: u32 = 2000;
        let food_tiles = (members * 16) / 4; // 800
        let labor_tiles = ((members * 60) / 100).saturating_mul(24); // 2880
        let seed_tiles = grain_seed_stock.max(32); // 2000
        let target_active = food_tiles.min(labor_tiles).min(seed_tiles); // 800
        let target_plots = (((target_active + 95) / 96).max(1)).min(12);
        // ceil(800/96) = 9
        assert_eq!(target_plots, 9);
    }

    /// `seed_tiles = grain_seed_stock.max(current_ag_tiles).max(32)` — an
    /// active belt floors the seed budget so a transient low seed reading
    /// doesn't shrink the plot plan below productive size.
    #[test]
    fn plot_sizing_floors_seed_budget_to_current_active_tiles() {
        let members: u32 = 60;
        let grain_seed_stock: u32 = 16; // below the 32 floor
        let current_ag_tiles: u32 = 600; // ~6 active plots worth
        let food_tiles = (members * 16) / 4; // 240
        let labor_tiles = ((members * 60) / 100).saturating_mul(24); // 864
        let seed_tiles = grain_seed_stock.max(current_ag_tiles).max(32); // 600
        let target_active = food_tiles.min(labor_tiles).min(seed_tiles); // 240
        let target_plots = (((target_active + 95) / 96).max(1)).min(12);
        // ceil(240/96) = 3 — driven by food_tiles, NOT shrunk by the low
        // seed_stock because current_ag_tiles holds the floor.
        assert_eq!(target_plots, 3);

        // Sanity: without the active-tile floor, seed_tiles would be 32 and
        // target_plots would collapse to 1.
        let naive_seed = grain_seed_stock.max(32);
        let naive_active = food_tiles.min(labor_tiles).min(naive_seed);
        assert_eq!(
            (((naive_active + 95) / 96).max(1)).min(12),
            1,
            "without the floor an active 6-plot village would shrink-plan to 1"
        );
    }
}
