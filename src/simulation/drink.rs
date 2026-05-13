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
use crate::simulation::faction::FactionMember;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::{Needs, DRINK_THIRST_REDUCTION, THIRST_SEVERE, THIRST_TRIGGER};
use crate::simulation::person::{AiState, Drafted, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, DrinkSource, Task};
use crate::world::biome::{water_kind_at, WaterKind};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::globe::Globe;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;

/// Game-ticks an agent stays in `Working` before the drink completes.
/// Short — drinking a sip is faster than eating a meal.
pub const TICKS_DRINK: u8 = 4;

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
            if (agent_tile.0 - tile.0).abs().max((agent_tile.1 - tile.1).abs()) > 1 {
                return DrinkOutcome::SourceGone;
            }
            let raw = !matches!(kind, TileKind::River);
            needs.thirst = (needs.thirst - DRINK_THIRST_REDUCTION).max(0.0);
            DrinkOutcome::Drank { raw }
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
                if matches!(water_kind_at(globe, kind, tile.0, tile.1), WaterKind::Salt) {
                    continue;
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
        if ai.task_id != TaskKind::Drink as u16 {
            continue;
        }
        let Some(source) = aq.current.as_drink() else {
            continue;
        };
        if ai.state != AiState::Working || ai.work_progress < TICKS_DRINK {
            continue;
        }

        let agent_tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );

        let outcome = perform_drink(
            source,
            &mut agent,
            &mut carrier,
            &mut needs,
            agent_tile,
            &chunk_map,
        );

        // Sickness roll: raw source or `SanitationMap`-contaminated tile.
        if let DrinkOutcome::Drank { raw } = outcome {
            let contaminated = match source {
                DrinkSource::Tile { tile } => sanitation.is_contaminated(tile),
                DrinkSource::Inventory => false,
            };
            let severity = if contaminated {
                crate::simulation::medicine::SICKNESS_CONTAMINATED_DRINK_SEVERITY
            } else if raw {
                crate::simulation::medicine::SICKNESS_RAW_DRINK_SEVERITY
            } else {
                0
            };
            if severity > 0 {
                if let Some(fresh) = crate::simulation::medicine::apply_sickness_severity(
                    sickness.as_deref_mut(),
                    severity,
                    now,
                ) {
                    commands.entity(entity).insert(fresh);
                }
            }
        }

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
        aq.advance();
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
            if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
                return;
            }
            if needs.thirst < THIRST_TRIGGER {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            // Method 1: drink from inventory if clean_water in hands or stack.
            let inv_clean = agent.quantity_of_resource(clean_water)
                + carrier.quantity_of_resource(clean_water);
            if inv_clean > 0 {
                ai.state = AiState::Working;
                ai.task_id = TaskKind::Drink as u16;
                ai.work_progress = 0;
                ai.dest_tile = (cur_tx, cur_ty);
                aq.dispatch(Task::Drink {
                    source: DrinkSource::Inventory,
                });
                return;
            }

            // Method 2: walk to nearest fresh-water tile.
            //
            // Severe-tier thirst widens the scan. Below severe the agent
            // gives up cleanly when no nearby source is visible; the chief
            // posting / boiling pipeline (Phase 6) will eventually surface
            // clean_water in storage.
            let scan = if needs.thirst >= THIRST_SEVERE {
                DRINK_TILE_SCAN_RADIUS * 2
            } else {
                DRINK_TILE_SCAN_RADIUS
            };
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
