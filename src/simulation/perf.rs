use bevy::prelude::*;
use std::collections::VecDeque;
use std::time::Duration;

/// Shared caps for background pipelines that must apply ECS/world mutations
/// on the main thread. Compute can run wider; apply stays deliberately small.
#[derive(Resource, Clone, Copy, Debug)]
pub struct PerfWorkBudget {
    pub chunk_data_loads_per_tick: usize,
    pub chunk_sprite_loads_per_tick: usize,
    pub chunk_unloads_per_tick: usize,
    pub graph_classify_chunks_per_task: usize,
    pub tile_refreshes_per_tick: usize,
    pub hotspot_rebuilds_per_tick: usize,
    pub world_sim_cells_per_task: usize,
    pub world_sim_deltas_per_tick: usize,
    pub job_faction_plan_applies_per_tick: usize,
    pub settlement_plan_applies_per_tick: usize,
    pub trader_market_arrivals_per_tick: usize,
    pub trader_market_plans_per_tick: usize,
    pub opportunity_rebuilds_per_tick: usize,
    pub herd_repulsion_rebuilds_per_tick: usize,
    pub diagnostics_samples_per_tick: usize,
    pub maintenance_factions_per_tick: usize,
    /// Per-tick cap on agents the awareness/wage gossip pass processes for
    /// the 7×7 neighbour merge. The snapshot pass stays full-population
    /// (cheap copy); the merge pass is the expensive O(slice × 49) part.
    pub gossip_agents_per_tick: usize,
    /// Per-tick cap on per-Person vision recomputes. Vision otherwise
    /// caches; only agents that moved or whose visible bbox was touched
    /// recompute, bounded by this cap to absorb mass-move spikes.
    pub vision_recomputes_per_tick: usize,
    /// Per-tick cap on inline-A\* replans in `animal_movement_system`. Animals
    /// keep stepping cached paths every tick (cheap); only the expensive A\*
    /// replan is budgeted — mirrors the person path-request worker's 64/tick.
    /// Round-robined by `AnimalReplanCursor` so no animal starves. Flow-field
    /// (HERD) replans are not counted (cheap).
    pub animal_replans_per_tick: usize,
    /// Per-tick cap on loose `GroundItem`s scanned for TTL expiry by
    /// `ground_item_decay_system`. Round-robined by `GroundItemDecayCursor`
    /// so every item is revisited within `ceil(N / cap)` ticks — far inside
    /// the shortest (2-day) TTL. No `tick % N` cadence.
    pub ground_item_decay_scans_per_tick: usize,
}

impl Default for PerfWorkBudget {
    fn default() -> Self {
        Self {
            chunk_data_loads_per_tick: 24,
            chunk_sprite_loads_per_tick: 8,
            chunk_unloads_per_tick: 32,
            graph_classify_chunks_per_task: 16,
            tile_refreshes_per_tick: 512,
            hotspot_rebuilds_per_tick: 1,
            // 512 * 256 cells over roughly 60 fixed ticks.
            world_sim_cells_per_task: 2_185,
            world_sim_deltas_per_tick: 2_185,
            job_faction_plan_applies_per_tick: 4,
            settlement_plan_applies_per_tick: 2,
            trader_market_arrivals_per_tick: 8,
            trader_market_plans_per_tick: 8,
            opportunity_rebuilds_per_tick: 16,
            herd_repulsion_rebuilds_per_tick: 2,
            diagnostics_samples_per_tick: 512,
            maintenance_factions_per_tick: 8,
            gossip_agents_per_tick: 64,
            vision_recomputes_per_tick: 32,
            animal_replans_per_tick: 64,
            ground_item_decay_scans_per_tick: 512,
        }
    }
}

/// Frame-local and rolling counters for background/budgeted work. The debug
/// panel reads this directly; systems update only the fields they own.
#[derive(Resource, Default, Debug)]
pub struct BackgroundWorkDiagnostics {
    pub pending_chunk_loads: u32,
    pub pending_chunk_sprite_loads: u32,
    pub pending_chunk_unloads: u32,
    pub chunk_loads_applied_last_tick: u32,
    pub chunk_sprite_loads_applied_last_tick: u32,
    pub chunk_unloads_applied_last_tick: u32,

    pub pending_tile_refreshes: u32,
    pub tile_refreshes_applied_last_tick: u32,

    pub graph_dirty_classify: u32,
    pub graph_dirty_unloaded: u32,
    pub graph_last_classify: u32,
    pub graph_last_edge_chunks: u32,
    pub graph_last_edges: u32,
    pub graph_compute_us: u32,
    pub graph_apply_us: u32,

    pub connectivity_in_flight: bool,
    pub connectivity_generation: u32,
    pub connectivity_compute_us: u32,
    pub connectivity_apply_us: u32,
    pub connectivity_dropped_stale: u64,

    pub hotspot_dirty: u32,
    pub hotspot_rebuilt_last_tick: u32,

    pub tile_change_chunks_last_tick: u32,

    pub world_sim_in_flight: bool,
    pub world_sim_cursor: u32,
    pub world_sim_snapshot_cells: u32,
    pub world_sim_pending_results: u32,
    pub world_sim_deltas_applied_last_tick: u32,
    pub world_sim_compute_us: u32,
    pub world_sim_apply_us: u32,
    pub world_sim_dropped_stale: u64,

    pub trader_snapshots_last_tick: u32,
    pub trader_arrivals_applied_last_tick: u32,
    pub trader_plans_installed_last_tick: u32,
    pub trader_backlog: u32,
    pub trader_apply_us: u32,

    pub opportunity_entries: u32,
    pub opportunity_dirty_factions: u32,
    pub opportunity_rebuilt_last_tick: u32,
    pub opportunity_full_rebuilds: u64,
    pub opportunity_apply_us: u32,

    pub herd_predators_indexed: u32,
    pub herd_clusters_scanned: u32,
    pub herd_repulsion_backlog: u32,
    pub herd_repulsion_built_last_tick: u32,
    pub herd_threat_scan_us: u32,

    pub diagnostics_samples_last_tick: u32,
    pub diagnostics_skipped_closed: u32,
    pub diagnostics_sample_us: u32,

    pub settlement_planner_backlog: u32,
    pub settlement_survey_in_flight: bool,
    pub settlement_survey_snapshot_chunks: u32,
    pub settlement_surveys_applied_last_tick: u32,
    pub settlement_survey_compute_us: u32,
    pub settlement_survey_apply_us: u32,
    pub settlement_survey_dropped_stale: u64,
}

#[inline]
pub fn micros_u32(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

/// Phase 1.2: per-faction stagger inside an every-tick loop.
///
/// Replaces the legacy `if clock.tick % CADENCE != 0 { return }` pattern that
/// concentrated every faction's work on the same tick. Each system runs every
/// tick but only processes a faction whose offset hits zero this tick — total
/// work per faction unchanged, but spread evenly across the cadence window.
///
/// `system_offset` deconflicts multiple systems sharing one CADENCE so all
/// factions don't pile their per-system spikes onto the same tick. Pick a
/// distinct const per call site (any value is fine; prime-ish spreads better).
#[inline]
pub fn faction_stagger_due(tick: u64, fid: u32, system_offset: u64, cadence: u64) -> bool {
    if cadence == 0 {
        return true;
    }
    tick.wrapping_add(fid as u64)
        .wrapping_add(system_offset)
        % cadence
        == 0
}

/// Return the next tick `≥ now` at which [`faction_stagger_due`] returns
/// true for the given `(fid, system_offset, cadence)`. Test helper: lets
/// tests fast-forward to the next fire instead of ticking a full cadence
/// window.
pub fn next_stagger_fire_tick(now: u64, fid: u32, system_offset: u64, cadence: u64) -> u64 {
    if cadence == 0 {
        return now;
    }
    let rem = now
        .wrapping_add(fid as u64)
        .wrapping_add(system_offset)
        % cadence;
    if rem == 0 {
        now
    } else {
        now + (cadence - rem)
    }
}

/// Per-system stagger offsets. Public so tests can compute the next firing
/// tick. Each system's offset is duplicated as a `const SYSTEM_OFFSET: u64`
/// at the call site for locality — keep these in sync if you change one.
pub mod stagger_offsets {
    pub const HUNTER: u64 = 17;
    pub const BUREAUCRAT: u64 = 37;
    pub const ARCHITECT: u64 = 53;
    pub const CRAFTER: u64 = 71;
    pub const FARMER: u64 = 89;
    pub const HEALER: u64 = 103;
    pub const NOMAD_SEDENTARIZE: u64 = 131;
    pub const NOMAD_CHIEF_DIRECTIVE: u64 = 149;
    pub const SEDENTARY_COLLAPSE: u64 = 167;
    pub const APPRENTICESHIP: u64 = 191;
}

// ── Offscreen fidelity preference ───────────────────────────────────────────
//
// Runtime knob for later-game scaling: how much simulation/streaming detail
// off-camera settled regions keep. Read by `update_simulation_focus_system`
// (whether/how far to emit a data-only `FocusPoint` per region) and
// `update_lod_levels_system` (how far a region centre promotes Dormant agents
// to Aggregate). The camera region is always fully live regardless.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffscreenFidelity {
    /// Every settled region fully streamed + simulated (original behaviour).
    AllLive,
    /// Camera region full; off-camera regions kept at a reduced radius and
    /// promote fewer agents. Remembered but cheaper. Default.
    Balanced,
    /// Off-camera regions emit no focus and stay Dormant — camera only.
    Minimal,
}

impl OffscreenFidelity {
    pub const ALL: [Self; 3] = [Self::AllLive, Self::Balanced, Self::Minimal];

    pub fn label(self) -> &'static str {
        match self {
            Self::AllLive => "All Live",
            Self::Balanced => "Balanced",
            Self::Minimal => "Minimal",
        }
    }

    /// Off-camera settled-region focus radius (chunks), or `None` to emit no
    /// off-camera region focus at all. `base` is the default
    /// `REGION_LOAD_RADIUS`.
    pub fn region_focus_radius(self, base: i32) -> Option<i32> {
        match self {
            Self::AllLive => Some(base),
            Self::Balanced => Some((base / 2).max(2)),
            Self::Minimal => None,
        }
    }

    /// Chunk radius around an off-camera region centre within which Dormant
    /// agents promote to Aggregate. 0 ⇒ no promotion.
    pub fn lod_promote_radius(self) -> i32 {
        match self {
            Self::AllLive => 8,
            Self::Balanced => 4,
            Self::Minimal => 0,
        }
    }
}

#[derive(Resource, Clone, Copy, Debug)]
pub struct PerformanceSettings {
    pub offscreen_fidelity: OffscreenFidelity,
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            offscreen_fidelity: OffscreenFidelity::Balanced,
        }
    }
}

// ── Growth-history sampling (Performance debug panel) ───────────────────────
//
// "Climbs over time" with everything bounded ⇒ explored-area-correlated cost
// (resource clusters, loaded chunks, gossip). To *identify* the climber
// empirically rather than guess, the Performance panel sparklines a handful of
// counters over game-time. This is pure analytics — sampled only while the
// debug panel is open, on a coarse cadence, so it adds no steady-state cost.

/// Ring length: number of retained samples per counter.
pub const PERF_HISTORY_CAP: usize = 120;
/// Spacing between samples, in fixed ticks (~600 ≈ a few game-minutes). Chosen
/// so the full ring spans a meaningful slice of year 1 without per-tick churn.
pub const PERF_SAMPLE_INTERVAL_TICKS: u64 = 600;

/// One counter's bounded sample ring. `delta()` (newest − oldest) is the
/// monotonic-growth signal; `max()` scales the sparkline.
#[derive(Default, Clone)]
pub struct PerfSeries {
    pub samples: VecDeque<u32>,
}

impl PerfSeries {
    pub fn push(&mut self, v: u32) {
        self.samples.push_back(v);
        if self.samples.len() > PERF_HISTORY_CAP {
            self.samples.pop_front();
        }
    }
    pub fn latest(&self) -> u32 {
        self.samples.back().copied().unwrap_or(0)
    }
    pub fn oldest(&self) -> u32 {
        self.samples.front().copied().unwrap_or(0)
    }
    /// Newest minus oldest retained sample — the climb signal.
    pub fn delta(&self) -> i64 {
        self.latest() as i64 - self.oldest() as i64
    }
    pub fn max(&self) -> u32 {
        self.samples.iter().copied().max().unwrap_or(0)
    }
}

/// Per-counter sample rings for the Performance panel. Populated by
/// [`perf_history_sample_system`]; read-only for the UI.
#[derive(Resource, Default)]
pub struct PerfHistory {
    pub ground_items: PerfSeries,
    pub job_postings: PerfSeries,
    pub blueprints: PerfSeries,
    pub loaded_chunks: PerfSeries,
    pub focus_points: PerfSeries,
    pub knowledge_clusters: PerfSeries,
    pub path_failures: PerfSeries,
    pub worldsim_pending: PerfSeries,
    pub last_sample_tick: u64,
}

/// Sample the growth-watch counters into `PerfHistory`. Early-exits when the
/// debug panel is closed (matches `sample_decision_metrics_system`'s gating);
/// when open, samples once per [`PERF_SAMPLE_INTERVAL_TICKS`]. The
/// elapsed-since-last check is a cursor, not a `tick % N` burst.
#[allow(clippy::too_many_arguments)]
pub fn perf_history_sample_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    debug_panel: Option<Res<crate::ui::debug_panel::DebugPanelState>>,
    mut hist: ResMut<PerfHistory>,
    job_board: Res<crate::simulation::jobs::JobBoard>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    focus: Res<crate::simulation::region::SimulationFocus>,
    shared: Res<crate::simulation::shared_knowledge::SharedKnowledge>,
    failures: Res<crate::pathfinding::path_request::FailureLog>,
    world_sim: Res<crate::simulation::world_sim::WorldSimTaskState>,
    ground_items: Query<(), With<crate::simulation::items::GroundItem>>,
    blueprints: Query<(), With<crate::simulation::construction::Blueprint>>,
) {
    if !debug_panel.map(|p| p.open).unwrap_or(false) {
        return;
    }
    // Seed on first open (last_sample_tick == 0), then space by interval.
    if hist.last_sample_tick != 0
        && clock.tick.saturating_sub(hist.last_sample_tick) < PERF_SAMPLE_INTERVAL_TICKS
    {
        return;
    }
    hist.last_sample_tick = clock.tick.max(1);

    hist.ground_items.push(ground_items.iter().count() as u32);
    hist.blueprints.push(blueprints.iter().count() as u32);
    hist.job_postings
        .push(job_board.postings.values().map(|v| v.len()).sum::<usize>() as u32);
    hist.loaded_chunks.push(chunk_map.0.len() as u32);
    hist.focus_points.push(focus.points.len() as u32);
    hist.knowledge_clusters.push(
        shared
            .tiers
            .values()
            .map(|m| m.clusters.len())
            .sum::<usize>() as u32,
    );
    hist.path_failures.push(failures.recent.len() as u32);
    hist.worldsim_pending.push(world_sim.pending_total() as u32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_series_rings_and_reports_delta() {
        let mut s = PerfSeries::default();
        for v in [3u32, 5, 4, 10] {
            s.push(v);
        }
        assert_eq!(s.oldest(), 3);
        assert_eq!(s.latest(), 10);
        assert_eq!(s.delta(), 7, "newest - oldest");
        assert_eq!(s.max(), 10);
    }

    #[test]
    fn perf_series_bounded_by_cap() {
        let mut s = PerfSeries::default();
        for v in 0..(PERF_HISTORY_CAP as u32 + 50) {
            s.push(v);
        }
        assert_eq!(
            s.samples.len(),
            PERF_HISTORY_CAP,
            "ring never exceeds its cap"
        );
        // Oldest retained sample dropped the first 50.
        assert_eq!(s.oldest(), 50);
        assert_eq!(s.latest(), PERF_HISTORY_CAP as u32 + 49);
    }

    #[test]
    fn offscreen_fidelity_region_radius_ladder() {
        let base = 6;
        assert_eq!(OffscreenFidelity::AllLive.region_focus_radius(base), Some(6));
        // Balanced halves (floored at 2).
        assert_eq!(
            OffscreenFidelity::Balanced.region_focus_radius(base),
            Some(3)
        );
        // Minimal emits no off-camera region focus.
        assert_eq!(OffscreenFidelity::Minimal.region_focus_radius(base), None);
    }

    #[test]
    fn offscreen_fidelity_promote_radius_ladder() {
        assert_eq!(OffscreenFidelity::AllLive.lod_promote_radius(), 8);
        assert_eq!(OffscreenFidelity::Balanced.lod_promote_radius(), 4);
        // Minimal never promotes off-camera agents out of Dormant.
        assert_eq!(OffscreenFidelity::Minimal.lod_promote_radius(), 0);
    }

    #[test]
    fn default_offscreen_fidelity_is_balanced() {
        assert_eq!(
            PerformanceSettings::default().offscreen_fidelity,
            OffscreenFidelity::Balanced
        );
    }
}
