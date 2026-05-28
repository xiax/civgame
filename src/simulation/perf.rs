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
