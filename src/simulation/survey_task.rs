//! Background-survey infrastructure for `SettlementBrain`.
//!
//! Surveys now run as a declarative async pipeline: the main thread captures
//! a bounded terrain/structure/member/faction snapshot, `AsyncComputeTaskPool`
//! computes a `SettlementBrain` + road-queue pushes, and the main thread
//! validates and applies at most `PerfWorkBudget::settlement_plan_applies_per_tick`
//! ready results per tick.

use ahash::AHashMap;
use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::simulation::construction::RoadCarveQueue;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::organic_settlement::{
    compute_settlement_survey, OrganicStructureMapsParam, SettlementBrains, SettlementParcelIndex,
    SettlementSurveyDiff, SettlementSurveyInput, SurveyFactionSnapshot, SurveyStructureSnapshot,
};
use crate::simulation::perf::{micros_u32, BackgroundWorkDiagnostics, PerfWorkBudget};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{Settlement, SettlementId};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::world_to_tile;

/// Half-side of the survey window (chebyshev tiles around the settlement
/// home tile). Must be ≥ `road_network_radius(Urban) ≈ 24` plus a healthy
/// buffer so the road / parcel sweeps see all anchors. Used by the
/// snapshot builders below.
pub const SURVEY_WINDOW: i32 = 64;

/// Chunk-map clone radius for async survey compute. Parcel belt allocation
/// can look farther than the anchor/structure window when a large built-up
/// footprint pushes fields outward, so the terrain snapshot is wider than
/// `SURVEY_WINDOW` while still bounded.
pub const SURVEY_TERRAIN_WINDOW: i32 = 192;

/// Cadence for survey passes. The scheduler advances one settlement every
/// `(ASYNC_SURVEY_INTERVAL / eligible_settlements).max(1)` ticks so each
/// eligible settlement is attempted about once per legacy 120-tick window.
pub const ASYNC_SURVEY_INTERVAL: u64 = 120;

/// Snapshot of all structure-map tiles within the survey window. Kept under
/// the historical name here; the concrete type lives with the organic survey
/// code that consumes it.
pub type MapsSnapshot = SurveyStructureSnapshot;

/// Per-member tile offset relative to `faction.home_tile`, captured before
/// the task starts so background survey compute never borrows the ECS world.
#[derive(Clone)]
pub struct MemberOffsets {
    pub offsets: Vec<(i32, i32)>,
}

/// Cursor index into the settlement set. The scheduler snapshots one
/// settlement per fire, walking through loaded settlements over time and
/// resetting each cycle.
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

pub struct SettlementSurveyTaskResult {
    pub diff: SettlementSurveyDiff,
    pub elapsed: Duration,
}

/// Async task state for settlement surveys. Tasks compute declarative diffs
/// off-thread; the main thread later validates and applies a small number of
/// ready results per tick.
#[derive(Resource, Default)]
pub struct SettlementSurveyTaskState {
    pub tasks: AHashMap<SettlementId, Task<SettlementSurveyTaskResult>>,
    pub ready: VecDeque<SettlementSurveyTaskResult>,
}

/// Cursor-paced async survey scheduler. It snapshots one eligible settlement
/// at a time and hands the pure survey compute to `AsyncComputeTaskPool`.
pub fn schedule_survey_tasks_system(
    clock: Res<SimClock>,
    mut cursor: ResMut<SurveyCursor>,
    mut state: ResMut<SettlementSurveyTaskState>,
    brains: Res<SettlementBrains>,
    settlements: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMapsParam,
    member_q: Query<(&FactionMember, &Transform)>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    // Snapshot eligible settlements in stable order.
    let mut eligible: Vec<&Settlement> = settlements
        .iter()
        .filter(|s| {
            registry
                .factions
                .get(&s.owner_faction)
                .map(|f| s.owner_faction != SOLO && f.caps.settlement.is_full_settlement())
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
    if cursor.last_advance_tick != 0
        && clock.tick.saturating_sub(cursor.last_advance_tick) < interval
    {
        return;
    }
    cursor.last_advance_tick = clock.tick;

    // Wrap the cursor and pick this tick's settlement.
    if cursor.next_index >= eligible.len() {
        cursor.next_index = 0;
    }
    let settlement = eligible[cursor.next_index];
    cursor.next_index = (cursor.next_index + 1) % eligible.len();

    if state.tasks.contains_key(&settlement.id)
        || state
            .ready
            .iter()
            .any(|ready| ready.diff.settlement_id == settlement.id)
    {
        update_survey_diagnostics(&state, &mut perf);
        return;
    }

    let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
        return;
    };
    let maps_view = maps.view();
    let terrain_snapshot = clone_chunk_window(&chunk_map, faction.home_tile);
    let snapshot_chunks = terrain_snapshot.0.len();
    let committed_ag_rects =
        snapshot_committed_ag_rects(&plot_index, &plot_q, settlement.id.0);
    let input = SettlementSurveyInput {
        settlement: settlement.clone(),
        faction: SurveyFactionSnapshot::from_faction(faction),
        tick: clock.tick,
        prior_brain: brains.0.get(&settlement.id).cloned(),
        chunk_map: terrain_snapshot,
        maps: MapsSnapshot::capture(&maps_view, faction.home_tile),
        member_offsets: snapshot_member_offsets(
            faction.home_tile,
            settlement.owner_faction,
            &member_q,
        )
        .offsets,
        snapshot_chunks,
        committed_ag_rects,
    };
    let pool = AsyncComputeTaskPool::get();
    let task = pool.spawn(async move {
        let started = Instant::now();
        let diff = compute_settlement_survey(input);
        SettlementSurveyTaskResult {
            diff,
            elapsed: started.elapsed(),
        }
    });
    state.tasks.insert(settlement.id, task);
    perf.settlement_survey_snapshot_chunks = snapshot_chunks.min(u32::MAX as usize) as u32;
    update_survey_diagnostics(&state, &mut perf);
}

pub fn poll_survey_tasks_system(
    mut state: ResMut<SettlementSurveyTaskState>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    let ids: Vec<SettlementId> = state.tasks.keys().copied().collect();
    let mut completed = Vec::new();
    for id in ids {
        let Some(task) = state.tasks.get_mut(&id) else {
            continue;
        };
        if let Some(result) = block_on(future::poll_once(task)) {
            completed.push((id, result));
        }
    }

    for (id, result) in completed {
        state.tasks.remove(&id);
        perf.settlement_survey_compute_us = micros_u32(result.elapsed);
        perf.settlement_survey_snapshot_chunks =
            result.diff.snapshot_chunks.min(u32::MAX as usize) as u32;
        state.ready.push_back(result);
    }
    update_survey_diagnostics(&state, &mut perf);
}

pub fn apply_survey_results_system(
    mut state: ResMut<SettlementSurveyTaskState>,
    mut brains: ResMut<SettlementBrains>,
    mut parcel_index: ResMut<SettlementParcelIndex>,
    mut road_queue: ResMut<RoadCarveQueue>,
    settlements: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    budget: Res<PerfWorkBudget>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    let started = Instant::now();
    let mut applied = 0u32;
    let max_results = budget.settlement_plan_applies_per_tick.max(1);

    for _ in 0..max_results {
        let Some(result) = state.ready.pop_front() else {
            break;
        };
        if !survey_result_is_current(&result.diff, &settlements, &registry) {
            perf.settlement_survey_dropped_stale =
                perf.settlement_survey_dropped_stale.saturating_add(1);
            continue;
        }
        for road_push in &result.diff.road_pushes {
            road_queue.0.push(road_push.clone());
        }
        brains
            .0
            .insert(result.diff.settlement_id, result.diff.brain);
        applied = applied.saturating_add(1);
    }

    if applied > 0 {
        parcel_index.rebuild(&brains);
    }
    perf.settlement_surveys_applied_last_tick = applied;
    perf.settlement_survey_apply_us = micros_u32(started.elapsed());
    update_survey_diagnostics(&state, &mut perf);
}

fn update_survey_diagnostics(
    state: &SettlementSurveyTaskState,
    perf: &mut BackgroundWorkDiagnostics,
) {
    perf.settlement_survey_in_flight = !state.tasks.is_empty();
    perf.settlement_planner_backlog =
        (state.tasks.len() + state.ready.len()).min(u32::MAX as usize) as u32;
}

fn survey_result_is_current(
    diff: &SettlementSurveyDiff,
    settlements: &Query<&Settlement>,
    registry: &FactionRegistry,
) -> bool {
    let Some(settlement) = settlements
        .iter()
        .find(|settlement| settlement.id == diff.settlement_id)
    else {
        return false;
    };
    let Some(faction) = registry.factions.get(&diff.owner_faction) else {
        return false;
    };
    survey_result_matches_live(diff, settlement, faction)
}

fn survey_result_matches_live(
    diff: &SettlementSurveyDiff,
    settlement: &Settlement,
    faction: &crate::simulation::faction::FactionData,
) -> bool {
    if settlement.owner_faction != diff.owner_faction
        || settlement.peak_population != diff.peak_population
    {
        return false;
    }
    diff.owner_faction != SOLO
        && faction.home_tile == diff.faction_home_tile
        && faction.caps.settlement.is_full_settlement()
}

pub fn clone_chunk_window(chunk_map: &ChunkMap, centre: (i32, i32)) -> ChunkMap {
    let centre_coord = ChunkCoord(
        centre.0.div_euclid(CHUNK_SIZE as i32),
        centre.1.div_euclid(CHUNK_SIZE as i32),
    );
    let radius_chunks =
        (SURVEY_TERRAIN_WINDOW + CHUNK_SIZE as i32 - 1).div_euclid(CHUNK_SIZE as i32);
    let mut out = ChunkMap::default();
    for (&coord, chunk) in &chunk_map.0 {
        if coord.chebyshev_dist(centre_coord) <= radius_chunks {
            out.0.insert(coord, chunk.clone());
        }
    }
    out
}

/// Helper: capture the rects of every Agricultural plot currently owned by
/// `settlement_id`. Used by the survey path to feed `build_ag_belt`'s
/// sticky pre-accept pass so committed farm land cannot be relocated by
/// the planner.
pub fn snapshot_committed_ag_rects(
    plot_index: &crate::simulation::land::PlotIndex,
    plot_q: &Query<&crate::simulation::land::Plot>,
    settlement_id: u32,
) -> Vec<crate::simulation::settlement::TileRect> {
    let mut out = Vec::new();
    let Some(pids) = plot_index.by_settlement.get(&settlement_id) else {
        return out;
    };
    for &pid in pids {
        let Some(&entity) = plot_index.by_id.get(&pid) else {
            continue;
        };
        let Ok(plot) = plot_q.get(entity) else {
            continue;
        };
        if plot.zone_kind == crate::simulation::settlement::ZoneKind::Agricultural {
            out.push(plot.rect);
        }
    }
    out
}

/// Helper: capture per-member tile offsets from a live transform query.
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
        let (tx, ty) = world_to_tile(t.translation.truncate());
        offsets.push((tx - hx, ty - hy));
    }
    MemberOffsets { offsets }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::market::SettlementMarket;
    use crate::simulation::organic_settlement::SettlementBrain;

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
    fn clone_chunk_window_empty_map_stays_empty() {
        let map = ChunkMap::default();
        let snap = clone_chunk_window(&map, (0, 0));
        assert!(snap.0.is_empty());
    }

    #[test]
    fn stale_survey_result_rejected_when_peak_population_changed() {
        let mut registry = FactionRegistry::default();
        let faction_id = registry.create_faction((4, 5));
        let faction = registry.factions.get(&faction_id).unwrap();
        let settlement = Settlement {
            id: SettlementId(7),
            owner_faction: faction_id,
            market_tile: (4, 5),
            founding_tick: 0,
            name: "test".to_string(),
            treasury: 0.0,
            market: SettlementMarket::default(),
            peak_population: 12,
            locality: None,
        };
        let diff = SettlementSurveyDiff {
            settlement_id: settlement.id,
            owner_faction: faction_id,
            faction_home_tile: faction.home_tile,
            peak_population: 11,
            tick: 1,
            brain: SettlementBrain::new(settlement.id, faction_id, 123),
            road_pushes: Vec::new(),
            snapshot_chunks: 0,
        };
        assert!(!survey_result_matches_live(&diff, &settlement, faction));
    }

    #[test]
    fn current_survey_result_is_accepted() {
        let mut registry = FactionRegistry::default();
        let faction_id = registry.create_faction((4, 5));
        let faction = registry.factions.get(&faction_id).unwrap();
        let settlement = Settlement {
            id: SettlementId(7),
            owner_faction: faction_id,
            market_tile: (4, 5),
            founding_tick: 0,
            name: "test".to_string(),
            treasury: 0.0,
            market: SettlementMarket::default(),
            peak_population: 12,
            locality: None,
        };
        let diff = SettlementSurveyDiff {
            settlement_id: settlement.id,
            owner_faction: faction_id,
            faction_home_tile: faction.home_tile,
            peak_population: settlement.peak_population,
            tick: 1,
            brain: SettlementBrain::new(settlement.id, faction_id, 123),
            road_pushes: Vec::new(),
            snapshot_chunks: 0,
        };
        assert!(survey_result_matches_live(&diff, &settlement, faction));
    }
}
