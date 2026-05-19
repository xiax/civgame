//! Drinking executor + helpers — Phase 2 of the thirst pipeline.
//!
//! Two drink modes:
//! - **Inventory drink** (`DrinkSource::Inventory`): no routing required;
//!   the agent stays in place and consumes one `clean_water` unit from
//!   inventory or hands.
//! - **Tile drink** (`DrinkSource::Tile { tile }`): the dispatcher routes
//!   the agent adjacent to a fresh-water tile (`River` / `Marsh` /
//!   inland `Water`) via the adjacency-routing path. On arrival, the
//!   executor verifies the source tile is still water and reduces
//!   thirst directly without consuming a resource.
//!
//! Salt-water tiles never produce a `Drink` dispatch: `DrinkAdjacentFreshTileMethod`
//! consults `world::biome::water_kind_at` to filter them out.
//!
//! Sickness from raw tile drinks is wired in Phase 5 via the shared
//! `apply_sickness` helper; today raw tile drinks succeed silently.

use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::core_ids;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::carry::Carrier;
use crate::simulation::construction::WellMap;
use crate::simulation::faction::FactionMember;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::{Needs, DRINK_THIRST_REDUCTION, THIRST_SEVERE, THIRST_TRIGGER};
use crate::simulation::person::{AiState, Drafted, PersonAI, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, DrinkSource, Task};
use crate::world::biome::water_kind_at;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::globe::Globe;
use crate::world::terrain::{GLOBE_H_TO_Z, TILE_SIZE};
use crate::world::tile::TileKind;

/// Game-ticks an agent stays in `Working` before the drink completes.
/// Short — drinking a sip is faster than eating a meal.
pub const TICKS_DRINK: u8 = 4;

/// Maximum sips consumed in a single Drink action. One sip reduces thirst by
/// `DRINK_THIRST_REDUCTION` (80); even a fully-thirsty agent (thirst 255)
/// drops to ~0 in 4 sips. Mirrors the multi-bite loop in `eat_task_system`
/// so a worker that walked to a river fully quenches before leaving instead
/// of sipping, idling, then being re-dispatched as thirst climbs back.
pub const MAX_SIPS_PER_ACTION: u32 = 4;

/// Stop drinking when thirst falls below this — the next sip would waste
/// more than 50% of its `DRINK_THIRST_REDUCTION`. Mirrors eat's
/// "majority waste" stop condition.
pub const DRINK_SATIETY_FLOOR: f32 = 40.0;

/// Resolve one drink for an agent. Returns true if the drink succeeded
/// (thirst reduced + resource consumed if applicable); false if the
/// source is no longer valid (clean_water missing / tile no longer
/// water). Public so tests + the chief stockpile path can reuse the
/// logic without going through the executor.
pub fn perform_drink(
    source: DrinkSource,
    agent: &mut EconomicAgent,
    carrier: &mut Carrier,
    needs: &mut Needs,
    agent_tile: (i32, i32),
    chunk_map: &ChunkMap,
    well_map: &WellMap,
    globe: &Globe,
) -> DrinkOutcome {
    match source {
        DrinkSource::Inventory => {
            let clean = core_ids::clean_water();
            let from_inv = agent.remove_resource(clean, 1);
            let consumed = if from_inv == 0 {
                carrier.remove_resource(clean, 1)
            } else {
                from_inv
            };
            if consumed == 0 {
                return DrinkOutcome::SourceGone;
            }
            needs.thirst = (needs.thirst - DRINK_THIRST_REDUCTION).max(0.0);
            DrinkOutcome::Drank { raw: false }
        }
        DrinkSource::Tile { tile } => {
            let nz = chunk_map.surface_z_at(tile.0, tile.1);
            let kind = chunk_map.tile_at(tile.0, tile.1, nz).kind;
            if !kind.is_drinkable_candidate() {
                return DrinkOutcome::SourceGone;
            }
            // Chebyshev adjacency to the source tile.
            if (agent_tile.0 - tile.0)
                .abs()
                .max((agent_tile.1 - tile.1).abs())
                > 1
            {
                return DrinkOutcome::SourceGone;
            }
            // River and bridged-river both expose fresh, non-raw water.
            let raw = !matches!(kind, TileKind::River | TileKind::Bridge);
            needs.thirst = (needs.thirst - DRINK_THIRST_REDUCTION).max(0.0);
            DrinkOutcome::Drank { raw }
        }
        DrinkSource::Well { tile } => {
            // Well must still exist (deconstruct cleared the map entry).
            if !well_map.0.contains_key(&tile) {
                return DrinkOutcome::SourceGone;
            }
            // Chebyshev adjacency to the well tile (the well itself is
            // impassable; the agent stands one step off and draws water).
            if (agent_tile.0 - tile.0)
                .abs()
                .max((agent_tile.1 - tile.1).abs())
                > 1
            {
                return DrinkOutcome::SourceGone;
            }
            // The shaft must still reach the water table. A well over a
            // deep/arid aquifer reads dry — graceful fail, the agent
            // re-plans (and the dispatcher already prefers wet wells).
            if !well_has_water(globe, chunk_map, tile) {
                return DrinkOutcome::WellDry;
            }
            // Well water is treated as clean; `SanitationMap` may still
            // mark it contaminated, handled by the caller.
            needs.thirst = (needs.thirst - DRINK_THIRST_REDUCTION).max(0.0);
            DrinkOutcome::Drank { raw: false }
        }
    }
}

/// Result of a `perform_drink` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrinkOutcome {
    /// Drink completed; `raw == true` when the source was non-river
    /// freshwater (lake / marsh) — caller should roll sickness when
    /// the Phase 5 pipeline is wired.
    Drank { raw: bool },
    /// Source resource / tile no longer valid; caller should `aq.cancel()`.
    SourceGone,
    /// The well's shaft doesn't reach the water table (arid / deep aquifer).
    /// Graceful: caller cancels the chain (no thirst reduction) so the agent
    /// re-plans to another source next dispatch — same handling as
    /// `SourceGone`, but distinct for clarity/telemetry.
    WellDry,
}

/// Radius in chebyshev tiles for the local fresh-water scan. Anchored to
/// the agent's tile; bigger than camp-radius so a thirsty agent can route
/// to a near-by stream without globe-wide search.
pub const DRINK_TILE_SCAN_RADIUS: i32 = 14;

/// Walk a chebyshev ring around `from` and return the closest fresh
/// (non-salt) drinkable water tile within `max_radius`. Filters out salt
/// `Water` tiles via `water_kind_at`. River tiles are always fresh; Marsh
/// is always fresh; inland-lake `Water` reads as `Fresh`; ocean `Water`
/// reads as `Salt` and is skipped.
pub fn nearest_fresh_drinkable_tile(
    chunk_map: &ChunkMap,
    globe: &Globe,
    from: (i32, i32),
    max_radius: i32,
) -> Option<(i32, i32)> {
    for r in 1..=max_radius {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue;
                }
                let tile = (from.0 + dx, from.1 + dy);
                let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if !kind.is_drinkable_candidate() {
                    continue;
                }
                if !water_kind_at(globe, kind, tile.0, tile.1).is_drinkable() {
                    continue; // skip Salt and Brackish
                }
                return Some(tile);
            }
        }
    }
    None
}

/// Executor for `TaskKind::Drink`. Mirrors `eat_task_system`: accumulates
/// `work_progress` to `TICKS_DRINK` while in `Working`, then calls
/// `perform_drink`. Runs in Sequential after movement.
pub fn drink_task_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    sanitation: Res<crate::simulation::sanitation::SanitationMap>,
    well_map: Res<WellMap>,
    globe: Res<Globe>,
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut ActionQueue,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Needs,
        &Transform,
        &BucketSlot,
        &LodLevel,
        Option<&mut crate::simulation::medicine::Sickness>,
    )>,
) {
    let now = clock.tick;
    for (
        entity,
        mut ai,
        mut aq,
        mut agent,
        mut carrier,
        mut needs,
        transform,
        slot,
        lod,
        mut sickness,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Drink as u16 {
            continue;
        }
        let Some(source) = aq.current.as_drink() else {
            // Defence in depth: legacy task_id says Drink but the typed
            // channel disagrees. Recover by dropping the chain so the next
            // dispatcher pass can re-plan, mirroring withdraw_good_task_system
            // (production.rs:323-327). Without this branch a desynced agent
            // freezes here forever — see the user-reported frozen-worker bug.
            aq.cancel_chain(&mut ai);
            continue;
        };
        if ai.state != AiState::Working || ai.work_progress < TICKS_DRINK {
            continue;
        }

        let agent_tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );

        // Multi-sip loop. Mirrors the multi-bite loop in `eat_task_system`:
        // keep drinking until quenched, until the source is exhausted (inventory
        // drink running out of `clean_water`), or until the next sip would be
        // majority waste. Tile / Well drinks don't exhaust here — the agent is
        // standing adjacent to the source — so the loop naturally drains thirst
        // in one dispatch.
        let mut sips: u32 = 0;
        let mut last_drank_raw: Option<bool> = None;
        while needs.thirst > DRINK_SATIETY_FLOOR && sips < MAX_SIPS_PER_ACTION {
            match perform_drink(
                source,
                &mut agent,
                &mut carrier,
                &mut needs,
                agent_tile,
                &chunk_map,
                &well_map,
                &globe,
            ) {
                DrinkOutcome::Drank { raw } => {
                    sips += 1;
                    last_drank_raw = Some(raw);
                }
                // Both terminate the sip loop without quenching; the typed
                // task then cancels and the agent re-plans (river fallback /
                // another well) on the next dispatch.
                DrinkOutcome::SourceGone | DrinkOutcome::WellDry => break,
            }
        }

        // Sickness roll: raw source or `SanitationMap`-contaminated tile.
        // Severity scales with sips taken — three sips of raw river water
        // is worse than one — capped at `u8::MAX` so the existing constants
        // still drive the per-sip step.
        if let Some(raw) = last_drank_raw {
            let contaminated = match source {
                DrinkSource::Tile { tile } => sanitation.is_contaminated(tile),
                DrinkSource::Well { tile } => sanitation.is_contaminated(tile),
                DrinkSource::Inventory => false,
            };
            let per_sip = if contaminated {
                crate::simulation::medicine::SICKNESS_CONTAMINATED_DRINK_SEVERITY
            } else if raw {
                crate::simulation::medicine::SICKNESS_RAW_DRINK_SEVERITY
            } else {
                0
            };
            if per_sip > 0 {
                let severity = (per_sip as u32).saturating_mul(sips).min(u8::MAX as u32) as u8;
                if let Some(fresh) = crate::simulation::medicine::apply_sickness_severity(
                    sickness.as_deref_mut(),
                    severity,
                    now,
                ) {
                    commands.entity(entity).insert(fresh);
                }
            }
        }

        aq.finish_task(&mut ai);
    }
}

/// Thirst pipeline dispatcher. Routes any `AgentGoal::Drink + Idle +
/// UNEMPLOYED` agent into a `Task::Drink`. Tries in-place inventory drink
/// first, then routes to the nearest fresh-water tile within
/// `DRINK_TILE_SCAN_RADIUS`. If neither succeeds, agent stays idle and
/// `goal_update_system` will re-score on its next 200-tick cadence.
///
/// Phase 2 keeps the dispatcher monolithic (no HTN Method registry walk)
/// because there are only two viable methods and they don't compete for
/// scoring nuance. A future phase can lift this into Method entries if
/// per-method history bias or wage-EV scoring become desirable.
pub fn htn_drink_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    globe: Res<Globe>,
    well_map: Res<WellMap>,
    mut query: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Needs,
            &EconomicAgent,
            &Carrier,
            &Transform,
            &FactionMember,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    let clean_water = core_ids::clean_water();
    query.par_iter_mut().for_each(
        |(mut ai, mut aq, goal, needs, agent, carrier, transform, _member, lod)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if *goal != AgentGoal::Drink {
                return;
            }
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
                return;
            }
            if needs.thirst < THIRST_TRIGGER {
                return;
            }
            // Normalize orphan ActionQueue. PersonAI being clean (Idle,
            // UNEMPLOYED) is supposed to imply aq.current == Idle, but
            // executors that exit with `aq.advance()` while queued is
            // non-empty leave `aq.current` pointing at the promoted-but-
            // unrouted next task. Subsequent `aq.dispatch(Task::Drink)`
            // can't promote (current != Idle) and Drink gets buried in
            // queued, leading to a frozen Working state because
            // `drink_task_system` reads `aq.current.as_drink()` → None.
            // Drop the orphan so promotion can proceed.
            if aq.current != Task::Idle {
                aq.cancel();
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            // Method 1: drink from inventory if clean_water in hands or stack.
            let inv_clean =
                agent.quantity_of_resource(clean_water) + carrier.quantity_of_resource(clean_water);
            if inv_clean > 0 {
                ai.state = AiState::Working;
                ai.work_progress = 0;
                ai.dest_tile = (cur_tx, cur_ty);
                aq.dispatch(Task::Drink {
                    source: DrinkSource::Inventory,
                });
                return;
            }

            let scan = if needs.thirst >= THIRST_SEVERE {
                DRINK_TILE_SCAN_RADIUS * 2
            } else {
                DRINK_TILE_SCAN_RADIUS
            };

            // Method 2: walk to the nearest well within scan. Wells beat
            // rivers because the dispatcher checks them first; settlements
            // with a well don't send their members on a multi-tile hike to
            // the riverbank for every sip. `TaskKind::Drink` routes via
            // `task_interacts_from_adjacent`, so passing the well tile here
            // lands the agent chebyshev-1 off it on the routing layer's
            // pick.
            if let Some(well_tile) =
                nearest_well_tile(&well_map, &globe, &chunk_map, (cur_tx, cur_ty), scan)
            {
                let routed = assign_task_with_routing(
                    &mut ai,
                    (cur_tx, cur_ty),
                    cur_chunk,
                    well_tile,
                    TaskKind::Drink,
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                );
                if routed {
                    aq.dispatch(Task::Drink {
                        source: DrinkSource::Well { tile: well_tile },
                    });
                    return;
                }
            }

            // Method 3: walk to nearest fresh-water tile.
            //
            // Severe-tier thirst widens the scan. Below severe the agent
            // gives up cleanly when no nearby source is visible; the chief
            // posting / boiling pipeline (Phase 6) will eventually surface
            // clean_water in storage.
            let Some(tile) =
                nearest_fresh_drinkable_tile(&chunk_map, &globe, (cur_tx, cur_ty), scan)
            else {
                return;
            };

            let routed = assign_task_with_routing(
                &mut ai,
                (cur_tx, cur_ty),
                cur_chunk,
                tile,
                TaskKind::Drink,
                None,
                &chunk_graph,
                &chunk_router,
                &chunk_map,
                &chunk_connectivity,
            );
            if routed {
                aq.dispatch(Task::Drink {
                    source: DrinkSource::Tile { tile },
                });
            }
        },
    );
}

/// A hand-dug well reaches this many Z-levels below the terrain surface
/// (≈6 m at 1.5 m/tile). The well yields water only when the local water
/// table (`HydroCell.aquifer_level`) sits within the dug shaft — a real
/// physical-reachability condition, not an arbitrary gate.
const WELL_REACH_Z: f32 = 4.0;

/// True iff a well at `tile` reaches the water table: the aquifer surface
/// (`HydroCell.aquifer_level` → Z via the single `GLOBE_H_TO_Z` factor) is
/// at or above the well-shaft bottom (`surface_z − WELL_REACH_Z`). A well
/// over a deep/arid water table reads dry. Falls back to "has water" when
/// the chunk/hydrology isn't resolvable (don't strand agents on a missing
/// cache read — the river fallback still applies downstream).
pub fn well_has_water(globe: &Globe, chunk_map: &ChunkMap, tile: (i32, i32)) -> bool {
    let surf = chunk_map.surface_z_at(tile.0, tile.1);
    if surf < crate::world::chunk::Z_MIN {
        return true; // chunk not loaded — defer to other drink methods
    }
    let Some(hc) = globe.hydro_cell_at(tile.0, tile.1) else {
        return true;
    };
    well_reaches(surf, hc.aquifer_level * GLOBE_H_TO_Z)
}

/// Pure shaft-vs-watertable test: the well bottom is `WELL_REACH_Z` below
/// the surface; it yields water iff the aquifer surface is at or above it.
#[inline]
pub fn well_reaches(surface_z: i32, aquifer_z: f32) -> bool {
    aquifer_z >= surface_z as f32 - WELL_REACH_Z
}

/// Chebyshev-nearest **water-bearing** well tile within `max_radius`. Dry
/// wells (aquifer below the shaft) are skipped so agents don't fixate on a
/// well that can't quench them — they fall through to the river method, and
/// a settlement whose wells are all dry reads as "fresh water far",
/// which already drives `SettlementPressureKind::WaterAccess`.
fn nearest_well_tile(
    well_map: &WellMap,
    globe: &Globe,
    chunk_map: &ChunkMap,
    from: (i32, i32),
    max_radius: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<((i32, i32), i32)> = None;
    for &well_tile in well_map.0.keys() {
        let d = (well_tile.0 - from.0)
            .abs()
            .max((well_tile.1 - from.1).abs());
        if d > max_radius {
            continue;
        }
        if !well_has_water(globe, chunk_map, well_tile) {
            continue;
        }
        if best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((well_tile, d));
        }
    }
    best.map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_reaches_water_table_within_shaft() {
        // Surface Z 10, shaft bottom = 10 - 4 = 6.
        assert!(well_reaches(10, 8.0), "high water table → wet well");
        assert!(well_reaches(10, 6.0), "table exactly at shaft bottom → wet");
        assert!(!well_reaches(10, 5.0), "table below shaft → dry well");
        // Arid: deep negative water table is unreachable.
        assert!(!well_reaches(0, -10.0), "deep arid aquifer → dry");
        assert!(well_reaches(0, 0.0), "table at surface → wet");
    }

    #[test]
    fn well_dry_is_distinct_graceful_outcome() {
        // Distinct from SourceGone for telemetry, but the caller treats
        // both as a non-quenching break (no thirst reduction).
        assert_ne!(DrinkOutcome::WellDry, DrinkOutcome::SourceGone);
    }
}
