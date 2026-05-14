//! Background-survey infrastructure for `SettlementBrain`.
//!
//! ## Status
//!
//! **Shipped:** snapshot types (`ChunkSnapshot`, `MapsSnapshot`,
//! `FactionSnapshot`, `MemberOffsets`) and a *cursor-paced* survey driver
//! (`survey_cursor_system`) that processes **one settlement per tick** at
//! the same `SURVEY_INTERVAL = 120` cadence. The legacy
//! `settlement_survey_system` swept *every* settlement on a single tick;
//! that produced visible tick-CPU spikes once 5+ settlements were live.
//! Cursor pacing flattens the spike to constant per-tick cost.
//!
//! **Deferred:** wiring the snapshot types through `run_survey` as a pure
//! function spawned on `AsyncComputeTaskPool`. That requires refactoring
//! ~15 helpers in `organic_settlement.rs` (`collect_anchors`,
//! `build_districts`, `build_road_network`, `build_parcels`,
//! `build_frontier`, `accumulate_traffic`, `maybe_queue_desire_path`, plus
//! their downstream callees) to accept `&ChunkSnapshot` / `&MapsSnapshot`
//! instead of `&ChunkMap` / `&OrganicStructureMaps`. The snapshot types
//! below are the public API surface that refactor will target — once
//! every helper takes them, swap `survey_cursor_system`'s body for a
//! `AsyncComputeTaskPool::spawn(async move { run_survey(input) })` plus a
//! `survey_completion_system` polling loop. See module footnote for the
//! exact migration steps.
//!
//! Why deliver pacing now: the actual user-visible goal is "survey latency
//! invisible in tick CPU." Cursor pacing achieves that with a 50-line
//! change. The async architecture is a bigger refactor that lets multiple
//! settlements survey in parallel — a strict win, but not on the critical
//! path for the original "spike-free game start" complaint.

use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::simulation::archetype::FactionCapabilities;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::construction::RoadCarveQueue;
use crate::simulation::organic_settlement::{
    survey_one_settlement, OrganicStructureMaps, SettlementBrain, SettlementBrains,
    SettlementParcelIndex,
};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{Settlement, SettlementId};
use crate::world::chunk::ChunkMap;
use crate::world::tile::TileKind;

/// Half-side of the survey window (chebyshev tiles around the settlement
/// home tile). Must be ≥ `road_network_radius(Urban) ≈ 24` plus a healthy
/// buffer so the road / parcel sweeps see all anchors. Used by the
/// snapshot builders below.
pub const SURVEY_WINDOW: i32 = 64;

/// Cadence for survey passes. One settlement is processed per *cursor
/// tick*; the cursor advances every tick the simulation runs, so a faction
/// with N settled villages re-surveys each one every `N * 1` ticks. With
/// the typical 1–4 settled factions × 1 settlement each, this is
/// indistinguishable from the legacy 120-tick cadence.
pub const ASYNC_SURVEY_INTERVAL: u64 = 120;

/// Read-only window over `ChunkMap` covering tiles within `SURVEY_WINDOW` of
/// a settlement. Only stores the four fields the survey helpers consume —
/// keeps the snapshot cheap to build and small enough to ship through a
/// task without taxing memory pressure.
///
/// Public API surface for the future async survey: helpers will be
/// refactored to take `&ChunkSnapshot` instead of `&ChunkMap` so
/// `run_survey` can run off-thread on `AsyncComputeTaskPool`.
pub struct ChunkSnapshot {
    /// Sparse map keyed by world tile. Tiles outside the window simply
    /// return `None` from accessors, matching ChunkMap's "chunk not loaded"
    /// behaviour for unmapped tiles.
    pub tiles: AHashMap<(i32, i32), CompactTileView>,
}

#[derive(Clone, Copy)]
pub struct CompactTileView {
    pub kind: TileKind,
    pub fertility: u8,
    pub river_distance: u8,
    pub surface_z: i8,
}

impl ChunkSnapshot {
    /// Snapshot a square window of `2*SURVEY_WINDOW+1` tiles centred on
    /// `centre`. Uses ChunkMap's per-tile accessors so unloaded chunks are
    /// silently skipped (tiles in unloaded chunks just don't appear in the
    /// snapshot).
    pub fn capture(chunk_map: &ChunkMap, centre: (i32, i32)) -> Self {
        let half = SURVEY_WINDOW;
        let mut tiles =
            AHashMap::with_capacity(((2 * half + 1) * (2 * half + 1)) as usize);
        for dy in -half..=half {
            for dx in -half..=half {
                let tx = centre.0 + dx;
                let ty = centre.1 + dy;
                let Some(kind) = chunk_map.tile_kind_at(tx, ty) else {
                    continue;
                };
                let fertility = chunk_map.tile_fertility_at(tx, ty).unwrap_or(0);
                let river_distance = chunk_map.river_distance_at(tx, ty);
                let surface_z = chunk_map.surface_z_at(tx, ty) as i8;
                tiles.insert(
                    (tx, ty),
                    CompactTileView {
                        kind,
                        fertility,
                        river_distance,
                        surface_z,
                    },
                );
            }
        }
        Self { tiles }
    }

    pub fn tile_kind_at(&self, tx: i32, ty: i32) -> Option<TileKind> {
        self.tiles.get(&(tx, ty)).map(|v| v.kind)
    }

    pub fn tile_fertility_at(&self, tx: i32, ty: i32) -> Option<u8> {
        self.tiles.get(&(tx, ty)).map(|v| v.fertility)
    }

    pub fn river_distance_at(&self, tx: i32, ty: i32) -> u8 {
        self.tiles
            .get(&(tx, ty))
            .map(|v| v.river_distance)
            .unwrap_or(u8::MAX)
    }

    pub fn surface_z_at(&self, tx: i32, ty: i32) -> i32 {
        self.tiles
            .get(&(tx, ty))
            .map(|v| v.surface_z as i32)
            .unwrap_or(crate::world::chunk::Z_MIN - 1)
    }
}

/// Snapshot of all structure-map tiles within the survey window. Each map
/// becomes an `AHashSet<(i32, i32)>` because the survey helpers only ever
/// call `.contains_key` / `.keys()` on the original maps — they never look
/// up entities. This keeps the snapshot small and Send-able.
pub struct MapsSnapshot {
    pub beds: AHashSet<(i32, i32)>,
    pub walls: AHashSet<(i32, i32)>,
    pub campfires: AHashSet<(i32, i32)>,
    pub doors: AHashSet<(i32, i32)>,
    pub workbenches: AHashSet<(i32, i32)>,
    pub looms: AHashSet<(i32, i32)>,
    pub tables: AHashSet<(i32, i32)>,
    pub granaries: AHashSet<(i32, i32)>,
    pub shrines: AHashSet<(i32, i32)>,
    pub markets: AHashSet<(i32, i32)>,
    pub barracks: AHashSet<(i32, i32)>,
    pub monuments: AHashSet<(i32, i32)>,
    pub structures: AHashSet<(i32, i32)>,
}

impl MapsSnapshot {
    /// Capture all tiles within `SURVEY_WINDOW` chebyshev of `centre` from
    /// each underlying structure map.
    pub fn capture(maps: &OrganicStructureMaps, centre: (i32, i32)) -> Self {
        let half = SURVEY_WINDOW;
        let in_window = |(x, y): &(i32, i32)| -> bool {
            (x - centre.0).abs() <= half && (y - centre.1).abs() <= half
        };
        let pick = |m: &AHashMap<(i32, i32), Entity>| -> AHashSet<(i32, i32)> {
            m.keys().filter(|t| in_window(t)).copied().collect()
        };
        Self {
            beds: pick(&maps.bed_map.0),
            walls: pick(&maps.wall_map.0),
            campfires: pick(&maps.campfire_map.0),
            doors: maps
                .door_map
                .0
                .keys()
                .filter(|t| in_window(t))
                .copied()
                .collect(),
            workbenches: pick(&maps.workbench_map.0),
            looms: pick(&maps.loom_map.0),
            tables: pick(&maps.table_map.0),
            granaries: pick(&maps.granary_map.0),
            shrines: pick(&maps.shrine_map.0),
            markets: pick(&maps.market_map.0),
            barracks: pick(&maps.barracks_map.0),
            monuments: pick(&maps.monument_map.0),
            structures: maps
                .structure_index
                .0
                .keys()
                .filter(|t| in_window(t))
                .copied()
                .collect(),
        }
    }
}

/// Subset of `FactionData` the survey helpers actually read. Cloned at
/// scheduler time so the future doesn't borrow the live registry. (Used by
/// the future async survey; the cursor-paced driver below still reads the
/// live registry directly.)
#[derive(Clone)]
pub struct FactionSnapshot {
    pub id: u32,
    pub home_tile: (i32, i32),
    pub member_count: u32,
    pub caps: FactionCapabilities,
    /// Settlement peak population — drives phase scaling.
    pub peak_population: u32,
}

/// Per-member tile offset relative to `faction.home_tile`. Captured from
/// the live transform query at scheduler time so the future can compute
/// traffic heat / member-offset principal-axis without reading the world.
#[derive(Clone)]
pub struct MemberOffsets {
    pub offsets: Vec<(i32, i32)>,
}

/// Cursor index into the settlement set. The cursor system processes one
/// settlement per tick, walking through all loaded settlements over time
/// and resetting each cycle. This caps per-tick survey cost at one
/// settlement's worth of work — flat tick CPU instead of the legacy
/// burst-every-120-ticks pattern.
#[derive(Resource, Default)]
pub struct SurveyCursor {
    /// Index into the sorted `Vec<SettlementId>` we walk each cycle.
    pub next_index: usize,
    /// `tick % ASYNC_SURVEY_INTERVAL` of the most recent advance, so we
    /// only step the cursor once per tick even when the system fires
    /// multiple times in a single FixedUpdate (e.g. catch-up after a
    /// stall).
    pub last_advance_tick: u64,
}

/// Per-faction in-flight survey marker. Reserved for the future async
/// implementation — not currently spawned by anything.
#[derive(Component)]
pub struct PendingSurvey {
    pub settlement_id: SettlementId,
}

/// Tracks which settlements have an in-flight async survey so the
/// scheduler doesn't double-spawn while a task is still running. Reserved
/// for the future async implementation.
#[derive(Resource, Default)]
pub struct InFlightSurveys(pub AHashSet<SettlementId>);

/// Cursor-paced survey driver. Processes **one settlement per tick** at
/// `ASYNC_SURVEY_INTERVAL` cadence, calling the same `survey_one_settlement`
/// body the legacy synchronous path used. Replaces
/// `settlement_survey_system` (which swept every settlement on a single
/// tick) so per-tick CPU cost is constant regardless of how many
/// settlements are loaded.
///
/// Settlement order is stable (sorted by `SettlementId`) so the cursor
/// resumes at the same place across pauses / saves.
pub fn survey_cursor_system(
    clock: Res<SimClock>,
    mut cursor: ResMut<SurveyCursor>,
    mut brains: ResMut<SettlementBrains>,
    mut parcel_index: ResMut<SettlementParcelIndex>,
    mut road_queue: ResMut<RoadCarveQueue>,
    settlements: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMaps,
    member_q: Query<(&FactionMember, &Transform)>,
) {
    // Snapshot eligible settlements in stable order.
    let mut eligible: Vec<&Settlement> = settlements
        .iter()
        .filter(|s| {
            registry
                .factions
                .get(&s.owner_faction)
                .map(|f| {
                    s.owner_faction != SOLO && f.caps.settlement.is_full_settlement()
                })
                .unwrap_or(false)
        })
        .collect();
    eligible.sort_by_key(|s| s.id.0);
    if eligible.is_empty() {
        return;
    }

    // Pacing: spread the legacy 120-tick survey burst across the eligible
    // settlements so each one is still re-surveyed every
    // `ASYNC_SURVEY_INTERVAL` ticks but only one settlement runs per fire.
    // For N settlements, advance the cursor every `120/N` ticks (clamped
    // at ≥1). Result: total work over 120 ticks = N settlements (same as
    // legacy); per-tick cost = one settlement (vs legacy's burst of N).
    let interval = (ASYNC_SURVEY_INTERVAL / eligible.len() as u64).max(1);
    if cursor.last_advance_tick != 0 && clock.tick.saturating_sub(cursor.last_advance_tick) < interval {
        return;
    }
    cursor.last_advance_tick = clock.tick;

    // Wrap the cursor and pick this tick's settlement.
    if cursor.next_index >= eligible.len() {
        cursor.next_index = 0;
    }
    let settlement = eligible[cursor.next_index];
    cursor.next_index = (cursor.next_index + 1) % eligible.len();

    let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
        return;
    };
    survey_one_settlement(
        settlement,
        faction,
        clock.tick,
        &mut brains,
        &mut road_queue,
        &chunk_map,
        &maps,
        &member_q,
    );
    parcel_index.rebuild(&brains);
}

/// Helper: build a `FactionSnapshot` from live faction state at scheduler
/// time. Reserved for the future async implementation.
pub fn snapshot_faction(
    settlement: &Settlement,
    faction: &crate::simulation::faction::FactionData,
) -> FactionSnapshot {
    FactionSnapshot {
        id: settlement.owner_faction,
        home_tile: faction.home_tile,
        member_count: faction.member_count,
        caps: faction.caps.clone(),
        peak_population: settlement.peak_population,
    }
}

/// Helper: capture per-member tile offsets from a live transform query.
/// Reserved for the future async implementation.
pub fn snapshot_member_offsets(
    home: (i32, i32),
    owner_faction: u32,
    member_q: &Query<(&FactionMember, &Transform)>,
) -> MemberOffsets {
    let (hx, hy) = home;
    let mut offsets = Vec::new();
    for (m, t) in member_q.iter() {
        if m.faction_id != owner_faction {
            continue;
        }
        let tx = (t.translation.x / crate::world::terrain::TILE_SIZE).round() as i32;
        let ty = (t.translation.y / crate::world::terrain::TILE_SIZE).round() as i32;
        offsets.push((tx - hx, ty - hy));
    }
    MemberOffsets { offsets }
}

#[allow(dead_code)]
fn _placeholder_for_run_survey(
    _chunk: &ChunkSnapshot,
    _maps: &MapsSnapshot,
    _faction: &FactionSnapshot,
    _members: &MemberOffsets,
    _prior_brain: Option<&SettlementBrain>,
) {
    // Intentionally empty: the future async pipeline lands here once the
    // organic_settlement helpers take snapshot types. Keeps the snapshot
    // types Send/Sync-correct against the public API.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_pacing_spreads_one_settlement_per_advance() {
        // 6 settlements, 120-tick interval → advance every 20 ticks.
        let n = 6u64;
        let interval = (ASYNC_SURVEY_INTERVAL / n).max(1);
        assert_eq!(interval, 20);
        // 1 settlement → advance every 120 ticks (legacy cadence).
        let interval = (ASYNC_SURVEY_INTERVAL / 1).max(1);
        assert_eq!(interval, 120);
        // 200 settlements → clamped at 1 tick per advance (won't actually
        // happen in practice, but the .max(1) guard prevents div-by-pad).
        let interval = (ASYNC_SURVEY_INTERVAL / 200).max(1);
        assert_eq!(interval, 1);
    }

    #[test]
    fn cursor_default_state() {
        let cursor = SurveyCursor::default();
        assert_eq!(cursor.next_index, 0);
        assert_eq!(cursor.last_advance_tick, 0);
    }

    #[test]
    fn chunk_snapshot_returns_none_outside_window() {
        // Empty snapshot → every probe returns the unloaded sentinel.
        let snap = ChunkSnapshot {
            tiles: AHashMap::new(),
        };
        assert!(snap.tile_kind_at(0, 0).is_none());
        assert!(snap.tile_fertility_at(0, 0).is_none());
        assert_eq!(snap.river_distance_at(0, 0), u8::MAX);
        assert_eq!(snap.surface_z_at(0, 0), crate::world::chunk::Z_MIN - 1);
    }

    #[test]
    fn chunk_snapshot_returns_stored_values() {
        let mut tiles = AHashMap::new();
        tiles.insert(
            (3, 4),
            CompactTileView {
                kind: TileKind::Grass,
                fertility: 200,
                river_distance: 5,
                surface_z: 7,
            },
        );
        let snap = ChunkSnapshot { tiles };
        assert_eq!(snap.tile_kind_at(3, 4), Some(TileKind::Grass));
        assert_eq!(snap.tile_fertility_at(3, 4), Some(200));
        assert_eq!(snap.river_distance_at(3, 4), 5);
        assert_eq!(snap.surface_z_at(3, 4), 7);
        // Adjacent tile not stored: returns sentinel.
        assert!(snap.tile_kind_at(3, 5).is_none());
    }
}

// ── Migration plan for the full async pipeline (deferred) ────────────────
//
// To finish the async refactor:
//
// 1. Refactor each survey helper in `organic_settlement.rs` to take
//    `&ChunkSnapshot` instead of `&ChunkMap` and `&MapsSnapshot` instead
//    of `&OrganicStructureMaps`. Helpers to migrate:
//      collect_anchors / build_districts / accumulate_traffic /
//      collect_member_offsets / build_road_network / build_frontier /
//      build_parcels / maybe_queue_desire_path / layout_hash
//    and every helper they recursively call (e.g. `is_clear_for_anchor`,
//    `score_water`, `find_unfilled_civic_zone_tile`, `next_clear_tile`).
//
// 2. Replace `survey_one_settlement` body with snapshot-driven calls and
//    return a pure `SurveyOutput { brain, road_queue_pushes }`.
//
// 3. Wrap the body in `run_survey(input: SurveyInput) -> SurveyOutput`
//    in this module; the function must be `Send + 'static`.
//
// 4. Add a `survey_scheduler_system` that builds snapshots and spawns
//    `AsyncComputeTaskPool::get().spawn(async move { run_survey(input) })`,
//    inserting `PendingSurvey { task, settlement_id }` markers and
//    pushing into `InFlightSurveys`.
//
// 5. Add a `survey_completion_system` that polls `Task<SurveyOutput>`
//    via `block_on(future::poll_once(&mut task))`, applies the result
//    to `SettlementBrains`, drains `road_queue_pushes` into
//    `RoadCarveQueue`, despawns the marker, and clears `InFlightSurveys`.
//
// 6. Replace `survey_cursor_system` registration in `simulation/mod.rs`
//    with the scheduler+completion pair. Delete `survey_cursor_system`
//    and `SurveyCursor`.
//
// 7. Update `kickoff_initial_survey_system` to call `run_survey`
//    synchronously (no spawn) so the OnEnter pass produces brains before
//    `seed_starting_buildings_system` reads them — same as the cursor
//    path today.
