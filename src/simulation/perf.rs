use bevy::prelude::*;
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

    pub faction_planner_backlog: u32,
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
