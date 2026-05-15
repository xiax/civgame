//! Pluralist Economy R10 follow-on: autonomous trader dispatch.
//!
//! Walks `Profession::Trader` agents through a deterministic
//! buy-low / sell-high cycle between two known settlements. Mirrors
//! the single-system shape of `bureaucrat_admin_dispatch_system` but
//! with phase state on a `TraderPlan` component so the trader can
//! progress through travel → trade → travel → trade without an HTN
//! method registration.
//!
//! **No new TaskKind / Task variant.** Travel between markets uses
//! `Task::Lead { dest }` (the same no-op-on-arrival primitive
//! bureaucrats stand on). The trade itself is a direct call to
//! `trader_buy_at_settlement` / `trader_sell_at_settlement` (R10
//! primitives) — currency moves atomically, never via a typed task.
//!
//! Two-system shape:
//! - `trader_market_step_system` (Economy, exclusive `&mut World`):
//!   on arrival at a phase's market tile, executes the trade,
//!   advances or clears the plan. Also seeds new plans on idle
//!   traders by scanning known settlements for arbitrage.
//! - `trader_route_dispatch_system` (ParallelB): for traders with a
//!   plan but not at the phase target, dispatches `Task::Lead`
//!   toward the destination via `assign_task_with_routing`. Mirrors
//!   `bureaucrat_admin_dispatch_system` exactly.
//!
//! **Hard guardrail intact:** zero diff in `tasks.rs` /
//! `typed_task.rs` / executor framework.

use ahash::AHashSet;
use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::economy::transactions::{trader_buy_at_settlement, trader_sell_at_settlement};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::AgentMemory;
use crate::simulation::person::{AiState, Drafted, PersonAI, Profession, TraderPhase, TraderPlan, UNEMPLOYED_TASK_KIND};
use crate::simulation::settlement::{Settlement, SettlementId, SettlementMap};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::TILE_SIZE;

/// Minimum currency a trader needs before considering a buy leg.
/// Below this, the dispatcher waits — there's no point committing to
/// a cycle the trader can't fund.
pub const TRADER_MIN_CAPITAL: f32 = 30.0;

/// Minimum price gap (per-unit) between cheap and expensive markets
/// before the trader commits to a cycle. Sub-threshold gaps don't
/// justify the walk cost. Calibrated so a typical 50-unit
/// supply/demand imbalance (price-update output around ±0.2) over a
/// shared catalog baseline of 1.0 just clears the gate.
pub const TRADER_MIN_GAP: f32 = 0.25;

/// Quantity per trade leg. Small — multiple cycles converge prices
/// faster than one big trade and respect treasury / inventory limits.
pub const TRADER_TRADE_QTY: u32 = 5;

/// Need-driven goals that preempt autonomous trader dispatch. When
/// the agent is in any of these, the dispatcher leaves them alone
/// and lets the goal's HTN chain run.
fn goal_preempts_trade(goal: &AgentGoal) -> bool {
    matches!(
        goal,
        AgentGoal::Survive
            | AgentGoal::Sleep
            | AgentGoal::Defend
            | AgentGoal::Raid
            | AgentGoal::Lead
            | AgentGoal::Rescue
            | AgentGoal::FollowingPlayerCommand
    )
}

/// Resolve a plan's settlements to (buy_tile, sell_tile, buy_entity,
/// sell_entity) when called from a non-exclusive system. Returns None
/// if either settlement was despawned.
fn resolve_plan_tiles_q(
    plan: &TraderPlan,
    map: &SettlementMap,
    settlements: &Query<&Settlement>,
) -> Option<((i32, i32), (i32, i32), Entity, Entity)> {
    let buy_e = *map.by_id.get(&plan.buy_settlement)?;
    let sell_e = *map.by_id.get(&plan.sell_settlement)?;
    let buy_tile = settlements.get(buy_e).ok()?.market_tile;
    let sell_tile = settlements.get(sell_e).ok()?.market_tile;
    Some((buy_tile, sell_tile, buy_e, sell_e))
}

/// Same as `resolve_plan_tiles_q` but for exclusive-system contexts
/// where the caller has `&World` and can read `Settlement` components
/// directly.
fn resolve_plan_tiles_world(
    plan: &TraderPlan,
    world: &World,
) -> Option<((i32, i32), (i32, i32), Entity, Entity)> {
    let map = world.resource::<SettlementMap>();
    let buy_e = *map.by_id.get(&plan.buy_settlement)?;
    let sell_e = *map.by_id.get(&plan.sell_settlement)?;
    let buy_tile = world.get::<Settlement>(buy_e)?.market_tile;
    let sell_tile = world.get::<Settlement>(sell_e)?.market_tile;
    Some((buy_tile, sell_tile, buy_e, sell_e))
}

/// Pick the best buy-low / sell-high pair across the trader's known
/// settlements for a single resource. Returns `(buy, sell)` if a
/// viable arbitrage exists, else `None`.
///
/// Viability gates:
/// - Both settlements registered in `SettlementMap`.
/// - Cheap market has ≥ qty stock of the resource.
/// - Expensive market's treasury can pay for ≥ qty * price.
/// - Gap > `TRADER_MIN_GAP`.
fn pick_arbitrage(
    visited: &[SettlementId],
    resource_id: ResourceId,
    qty: u32,
    world: &World,
) -> Option<(SettlementId, SettlementId)> {
    let map = world.resource::<SettlementMap>();
    // Single pass: snapshot (price, stock, treasury) once per settlement,
    // then run the O(N²) gap scan over the snapshot. Eliminates redundant
    // `world.get::<Settlement>()` lookups from the inner loop.
    struct Snap {
        id: SettlementId,
        price: f32,
        stock: f32,
        treasury: f32,
    }
    let mut snaps: Vec<Snap> = Vec::with_capacity(visited.len());
    for &id in visited.iter() {
        let entity = match map.by_id.get(&id) {
            Some(e) => *e,
            None => continue,
        };
        let s = match world.get::<Settlement>(entity) {
            Some(s) => s,
            None => continue,
        };
        snaps.push(Snap {
            id,
            price: s.market.price_of(resource_id),
            stock: s.market.stock_of(resource_id),
            treasury: s.treasury,
        });
    }

    let mut best: Option<(SettlementId, SettlementId, f32)> = None;
    for cheap in &snaps {
        if cheap.stock < qty as f32 {
            continue;
        }
        for expensive in &snaps {
            if expensive.id == cheap.id {
                continue;
            }
            let gap = expensive.price - cheap.price;
            if gap < TRADER_MIN_GAP {
                continue;
            }
            if expensive.treasury < expensive.price * qty as f32 {
                continue;
            }
            match best {
                Some((_, _, prev_gap)) if prev_gap >= gap => {}
                _ => best = Some((cheap.id, expensive.id, gap)),
            }
        }
    }
    best.map(|(c, e, _)| (c, e))
}

/// Snapshot of a trader's relevant state captured before mutation.
/// Built in a parallel-safe pre-pass; the mutation pass walks this
/// list and `world.get_*` to apply trades / install plans.
struct TraderSnapshot {
    entity: Entity,
    tile: (i32, i32),
    plan: Option<TraderPlan>,
    aq_current_idle: bool,
    task_unemployed: bool,
    visited: Vec<SettlementId>,
    currency: f32,
}

/// Pluralist Economy R10 follow-on: trade execution + plan
/// creation. Exclusive system because `trader_buy_at_settlement` /
/// `trader_sell_at_settlement` need `&mut World`.
///
/// On arrival at a buy market with `Phase::TravelingToBuy`: executes
/// the buy, advances to `Phase::TravelingToSell`, and cancels `aq` so
/// the route dispatcher seeds a new Lead task next tick. On arrival
/// at the sell market: executes the sell and removes the plan.
///
/// For idle traders without a plan: scans known settlements for an
/// arbitrage opportunity and installs a `TraderPlan` so the route
/// dispatcher will route them.
pub fn trader_market_step_system(world: &mut World) {
    // ── Pass 1: snapshot relevant trader state ───────────────────
    let snapshots: Vec<TraderSnapshot> = {
        let mut q = world.query::<(
            Entity,
            &Profession,
            &PersonAI,
            &ActionQueue,
            &Transform,
            &LodLevel,
            &AgentGoal,
            &AgentMemory,
            &EconomicAgent,
            Option<&TraderPlan>,
            Option<&Drafted>,
        )>();
        let mut out = Vec::new();
        for (entity, prof, ai, aq, transform, lod, goal, memory, econ, plan, drafted) in
            q.iter(world)
        {
            if *prof != Profession::Trader {
                continue;
            }
            if *lod == LodLevel::Dormant {
                continue;
            }
            if drafted.is_some() {
                continue;
            }
            if goal_preempts_trade(goal) {
                continue;
            }
            let tile = (
                (transform.translation.x / TILE_SIZE).floor() as i32,
                (transform.translation.y / TILE_SIZE).floor() as i32,
            );
            out.push(TraderSnapshot {
                entity,
                tile,
                plan: plan.copied(),
                aq_current_idle: matches!(aq.current, Task::Idle),
                task_unemployed: aq.current_task_kind() == UNEMPLOYED_TASK_KIND,
                visited: memory.known_settlements().map(|(id, _)| id).collect(),
                currency: econ.currency,
            });
            // Suppress unused-binding warnings on snapshot-only fields.
            let _ = ai;
        }
        out
    };

    if snapshots.is_empty() {
        return;
    }

    // ── Pass 2: trade-on-arrival + plan creation ─────────────────
    for snap in snapshots {
        // Re-resolve plan tile lookups via a query each iteration —
        // settlements rarely change tile and plans are short-lived,
        // so cost is negligible.
        if let Some(plan) = snap.plan {
            let resolved = resolve_plan_tiles_world(&plan, world);
            let Some((buy_tile, sell_tile, buy_e, sell_e)) = resolved else {
                world.entity_mut(snap.entity).remove::<TraderPlan>();
                continue;
            };

            match plan.phase {
                TraderPhase::TravelingToBuy if snap.tile == buy_tile => {
                    let bought = trader_buy_at_settlement(
                        world,
                        snap.entity,
                        buy_e,
                        plan.resource_id,
                        plan.qty,
                    );
                    if bought.is_some() {
                        if let Some(mut tp) = world.get_mut::<TraderPlan>(snap.entity) {
                            tp.phase = TraderPhase::TravelingToSell;
                        }
                    } else {
                        // Buy failed (insufficient stock / funds).
                        // Drop plan; the next tick's idle scan picks
                        // another pair (or waits for treasury).
                        world.entity_mut(snap.entity).remove::<TraderPlan>();
                    }
                    if let Some(mut aq) = world.get_mut::<ActionQueue>(snap.entity) {
                        aq.cancel();
                    }
                    if let Some(mut ai) = world.get_mut::<PersonAI>(snap.entity) {
                        ai.state = AiState::Idle;
                    }
                    continue;
                }
                TraderPhase::TravelingToSell if snap.tile == sell_tile => {
                    let _ = trader_sell_at_settlement(
                        world,
                        snap.entity,
                        sell_e,
                        plan.resource_id,
                        plan.qty,
                    );
                    world.entity_mut(snap.entity).remove::<TraderPlan>();
                    if let Some(mut aq) = world.get_mut::<ActionQueue>(snap.entity) {
                        aq.cancel();
                    }
                    if let Some(mut ai) = world.get_mut::<PersonAI>(snap.entity) {
                        ai.state = AiState::Idle;
                    }
                    continue;
                }
                _ => continue, // in transit
            }
        }

        // No plan: only act if fully idle and well-capitalised.
        if !snap.aq_current_idle || !snap.task_unemployed {
            continue;
        }
        if snap.currency < TRADER_MIN_CAPITAL {
            continue;
        }
        let dedup: AHashSet<SettlementId> = snap.visited.iter().copied().collect();
        if dedup.len() < 2 {
            continue;
        }
        // V1: arbitrage Cloth only. Future versions can iterate
        // over more resources and pick the best gap; for the
        // validation gate, one resource is enough to prove the
        // cycle.
        let resource_id = core_ids::cloth();
        let qty = TRADER_TRADE_QTY;
        let visited_vec: Vec<SettlementId> = dedup.into_iter().collect();
        let pick = pick_arbitrage(&visited_vec, resource_id, qty, world);
        let Some((buy, sell)) = pick else {
            continue;
        };
        world.entity_mut(snap.entity).insert(TraderPlan {
            phase: TraderPhase::TravelingToBuy,
            buy_settlement: buy,
            sell_settlement: sell,
            resource_id,
            qty,
        });
    }
}

/// Pluralist Economy R10 follow-on: route a plan-bearing trader
/// toward the current phase's market tile via
/// `assign_task_with_routing`. Mirrors `bureaucrat_admin_dispatch_system`
/// — same gate (Profession + idle aq + UNEMPLOYED task), same routing
/// shape (`TaskKind::Lead`).
///
/// Runs in `SimulationSet::ParallelB` after the combat dispatcher so
/// need-driven goals (Survive / Defend / etc.) preempt naturally.
pub fn trader_route_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    settlement_map: Res<SettlementMap>,
    settlements: Query<&Settlement>,
    mut query: Query<
        (
            &Profession,
            &TraderPlan,
            &mut PersonAI,
            &mut ActionQueue,
            &Transform,
            &LodLevel,
            &AgentGoal,
        ),
        Without<Drafted>,
    >,
) {
    for (prof, plan, mut ai, mut aq, transform, lod, goal) in query.iter_mut() {
        if *prof != Profession::Trader {
            continue;
        }
        if *lod == LodLevel::Dormant {
            continue;
        }
        if goal_preempts_trade(goal) {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if !matches!(aq.current, Task::Idle) {
            continue;
        }

        let Some((buy_tile, sell_tile, _, _)) =
            resolve_plan_tiles_q(plan, &settlement_map, &settlements)
        else {
            continue;
        };
        let dest = match plan.phase {
            TraderPhase::TravelingToBuy => buy_tile,
            TraderPhase::TravelingToSell => sell_tile,
        };

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        if (cur_tx, cur_ty) == dest {
            // Already at destination: market_step_system handles the
            // trade next exclusive pass.
            aq.dispatch(Task::Lead { dest });
            continue;
        }
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let routed = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            dest,
            TaskKind::Lead,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if routed {
            aq.dispatch(Task::Lead { dest });
        }
    }
}
