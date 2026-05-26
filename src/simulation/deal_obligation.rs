//! Smart-diplomacy P3 — `DealObligation` entities + courier pipeline.
//!
//! When a `DealPackage` is accepted, each transfer term (resource or
//! currency) spawns one `DealObligation` entity holding the payload
//! and a deadline. A courier (Trader > Bureaucrat > idle adult, sourced
//! from the grantor faction) is assigned to walk the obligation to the
//! recipient. On arrival the payload is delivered atomically; past
//! deadline the obligation defaults and refunds the grantor while
//! logging `IncidentKind::DealDefaulted`.
//!
//! Couriers use the same primitive Traders use — `Task::Lead { dest }`
//! while idle. No new `AgentGoal` / HTN method; the `deal_courier_route_dispatch_system`
//! re-stamps the task whenever the courier is idle, mirroring
//! `trader_route_dispatch_system`'s shape.

use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::diplomacy::{
    record_incident, DealId, DiplomacyLedger, IncidentKind, ObligationPayload, PendingObligation,
};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI, Profession, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::SimClock;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::terrain::TILE_SIZE;

/// Status of a `DealObligation`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ObligationStatus {
    /// Payload debited from grantor storage; no courier yet.
    Pending,
    /// Courier picked + en route.
    InTransit { courier: Entity },
    /// Delivered (about to despawn).
    Delivered,
    /// Past deadline; refund happened.
    Defaulted,
}

#[derive(Component, Clone, Debug)]
pub struct DealObligation {
    pub deal_id: DealId,
    pub from_faction: u32,
    pub to_faction: u32,
    pub payload: ObligationPayload,
    pub deadline_tick: u64,
    pub status: ObligationStatus,
    /// Destination tile resolved at spawn time from recipient's
    /// `Settlement.market_tile` or `FactionData.home_tile`.
    pub dest_tile: (i32, i32),
}

/// Marker on the carrier agent. Walks via Task::Lead while alive.
#[derive(Component, Clone, Debug)]
pub struct DealCourier {
    pub obligation: Entity,
}

/// Arrival radius for "courier reached destination".
pub const COURIER_ARRIVAL_RADIUS: i32 = 6;

/// Spawn a `DealObligation` entity from a `PendingObligation`, debiting
/// the grantor's storage / treasury. Returns `Some(Entity)` on success,
/// `None` when the grantor can't actually deliver (block fires).
pub fn spawn_obligation(
    commands: &mut Commands,
    registry: &mut FactionRegistry,
    pending: &PendingObligation,
    settlement_map: &crate::simulation::settlement::SettlementMap,
    settlement_q: &Query<&crate::simulation::settlement::Settlement>,
) -> Option<Entity> {
    let dest_tile = resolve_dest_tile(pending.to_faction, registry, settlement_map, settlement_q)
        .unwrap_or((0, 0));
    // Debit grantor up-front. If grantor lacks the payload, drop the
    // obligation silently (the package shouldn't have been accepted).
    let Some(grantor) = registry.factions.get_mut(&pending.from_faction) else {
        return None;
    };
    match pending.payload {
        ObligationPayload::Resource { resource_id, qty } => {
            let rid = ResourceId(resource_id);
            let have = grantor.storage.totals.get(&rid).copied().unwrap_or(0);
            if have < qty {
                return None;
            }
            grantor.storage.totals.insert(rid, have - qty);
        }
        ObligationPayload::Currency { amount } => {
            if grantor.treasury < amount as f32 {
                return None;
            }
            grantor.treasury -= amount as f32;
        }
    }
    let entity = commands
        .spawn(DealObligation {
            deal_id: pending.deal_id,
            from_faction: pending.from_faction,
            to_faction: pending.to_faction,
            payload: pending.payload,
            deadline_tick: pending.deadline_tick,
            status: ObligationStatus::Pending,
            dest_tile,
        })
        .id();
    Some(entity)
}

fn resolve_dest_tile(
    faction_id: u32,
    registry: &FactionRegistry,
    settlement_map: &crate::simulation::settlement::SettlementMap,
    settlement_q: &Query<&crate::simulation::settlement::Settlement>,
) -> Option<(i32, i32)> {
    if let Some(sid) = settlement_map.first_for_faction(faction_id) {
        if let Some(e) = settlement_map.by_id.get(&sid) {
            if let Ok(s) = settlement_q.get(*e) {
                return Some(s.market_tile);
            }
        }
    }
    registry.factions.get(&faction_id).map(|d| d.home_tile)
}

/// Daily Economy pass — assignment + arrival + default.
pub fn deal_obligation_step_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut registry: ResMut<FactionRegistry>,
    mut ledger: ResMut<DiplomacyLedger>,
    mut obligations: Query<(Entity, &mut DealObligation)>,
    candidate_workers: Query<(
        Entity,
        &Transform,
        &FactionMember,
        &PersonAI,
        &ActionQueue,
        &LodLevel,
        Option<&Profession>,
        Option<&Drafted>,
        Option<&DealCourier>,
    )>,
    carriers_q: Query<(Entity, &Transform)>,
) {
    if clock.tick == 0 || clock.tick % (TICKS_PER_DAY as u64 / 4) != 0 {
        return;
    }
    let now = clock.tick;
    // Pre-pass: snapshot every potential carrier.
    struct Cand {
        entity: Entity,
        tile: (i32, i32),
        faction: u32,
        prof_score: i32,
    }
    let mut cands: Vec<Cand> = Vec::new();
    for (e, t, member, ai, aq, lod, prof, drafted, dc) in candidate_workers.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        if drafted.is_some() || dc.is_some() {
            continue;
        }
        if !matches!(*lod, LodLevel::Full) {
            continue;
        }
        if !matches!(ai.state, AiState::Idle) {
            continue;
        }
        if !matches!(aq.current, Task::Idle) {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        let tile = (
            (t.translation.x / TILE_SIZE).floor() as i32,
            (t.translation.y / TILE_SIZE).floor() as i32,
        );
        let prof_score = match prof {
            Some(Profession::Trader) => 3,
            Some(Profession::Bureaucrat) => 2,
            _ => 1,
        };
        cands.push(Cand {
            entity: e,
            tile,
            faction: member.faction_id,
            prof_score,
        });
    }

    let mut taken: ahash::AHashSet<Entity> = ahash::AHashSet::new();

    // ── Walk obligations ─────────────────────────────────────────────
    let mut to_despawn: Vec<Entity> = Vec::new();
    for (entity, mut ob) in obligations.iter_mut() {
        match ob.status {
            ObligationStatus::Defaulted | ObligationStatus::Delivered => {
                to_despawn.push(entity);
                continue;
            }
            ObligationStatus::Pending => {
                // Past deadline? default + refund.
                if now >= ob.deadline_tick {
                    refund_payload(&mut registry, ob.from_faction, ob.payload);
                    record_incident(
                        &mut ledger,
                        ob.from_faction,
                        ob.to_faction,
                        now,
                        IncidentKind::DealDefaulted { deal_id: ob.deal_id.0 },
                    );
                    ob.status = ObligationStatus::Defaulted;
                    to_despawn.push(entity);
                    continue;
                }
                // Pick best courier in grantor faction.
                let Some(pick) = cands
                    .iter()
                    .filter(|c| c.faction == ob.from_faction && !taken.contains(&c.entity))
                    .max_by_key(|c| c.prof_score)
                    .map(|c| c.entity)
                else {
                    continue;
                };
                taken.insert(pick);
                commands
                    .entity(pick)
                    .insert(DealCourier { obligation: entity });
                ob.status = ObligationStatus::InTransit { courier: pick };
            }
            ObligationStatus::InTransit { courier } => {
                // Default check first
                if now >= ob.deadline_tick {
                    refund_payload(&mut registry, ob.from_faction, ob.payload);
                    record_incident(
                        &mut ledger,
                        ob.from_faction,
                        ob.to_faction,
                        now,
                        IncidentKind::DealDefaulted { deal_id: ob.deal_id.0 },
                    );
                    commands.entity(courier).remove::<DealCourier>();
                    ob.status = ObligationStatus::Defaulted;
                    to_despawn.push(entity);
                    continue;
                }
                // Carrier alive?
                let Ok((_, t)) = carriers_q.get(courier) else {
                    // Courier died — return to Pending, awaiting new pick.
                    ob.status = ObligationStatus::Pending;
                    continue;
                };
                let tx = (t.translation.x / TILE_SIZE).floor() as i32;
                let ty = (t.translation.y / TILE_SIZE).floor() as i32;
                let dx = (tx - ob.dest_tile.0).abs();
                let dy = (ty - ob.dest_tile.1).abs();
                if dx.max(dy) <= COURIER_ARRIVAL_RADIUS {
                    // Deliver atomically.
                    deliver_payload(&mut registry, ob.to_faction, ob.payload);
                    record_incident(
                        &mut ledger,
                        ob.from_faction,
                        ob.to_faction,
                        now,
                        match ob.payload {
                            ObligationPayload::Resource { resource_id, qty } => {
                                IncidentKind::DealDelivered { deal_id: ob.deal_id.0, resource_id, qty }
                            }
                            ObligationPayload::Currency { amount } => IncidentKind::DealDelivered {
                                deal_id: ob.deal_id.0,
                                resource_id: u16::MAX,
                                qty: amount,
                            },
                        },
                    );
                    commands.entity(courier).remove::<DealCourier>();
                    ob.status = ObligationStatus::Delivered;
                    to_despawn.push(entity);
                }
            }
        }
    }
    for e in to_despawn {
        commands.entity(e).despawn();
    }
}

fn refund_payload(registry: &mut FactionRegistry, faction: u32, payload: ObligationPayload) {
    let Some(d) = registry.factions.get_mut(&faction) else {
        return;
    };
    match payload {
        ObligationPayload::Resource { resource_id, qty } => {
            let rid = ResourceId(resource_id);
            let have = d.storage.totals.get(&rid).copied().unwrap_or(0);
            d.storage.totals.insert(rid, have.saturating_add(qty));
        }
        ObligationPayload::Currency { amount } => {
            d.treasury += amount as f32;
        }
    }
}

fn deliver_payload(registry: &mut FactionRegistry, faction: u32, payload: ObligationPayload) {
    let Some(d) = registry.factions.get_mut(&faction) else {
        return;
    };
    match payload {
        ObligationPayload::Resource { resource_id, qty } => {
            let rid = ResourceId(resource_id);
            let have = d.storage.totals.get(&rid).copied().unwrap_or(0);
            d.storage.totals.insert(rid, have.saturating_add(qty));
        }
        ObligationPayload::Currency { amount } => {
            d.treasury += amount as f32;
        }
    }
}

/// ParallelB — route active couriers via `Task::Lead { dest }`.
/// Mirrors `trader::trader_route_dispatch_system`.
pub fn deal_courier_route_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    obligations: Query<&DealObligation>,
    mut query: Query<
        (
            Entity,
            &DealCourier,
            &mut PersonAI,
            &mut ActionQueue,
            &Transform,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
    spatial_index: Res<crate::world::spatial::SpatialIndex>,
    stand_reservations: Res<crate::simulation::stand_reservation::StandTileReservations>,
    clock: Res<SimClock>,
) {
    let now = clock.tick;
    for (actor, dc, mut ai, mut aq, transform, lod) in query.iter_mut() {
        if matches!(*lod, LodLevel::Dormant) {
            continue;
        }
        if !matches!(aq.current, Task::Idle) {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        let Ok(ob) = obligations.get(dc.obligation) else {
            continue;
        };
        let dest = ob.dest_tile;
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        if (cur_tx, cur_ty) == dest {
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
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
            &spatial_index,
            &stand_reservations,
            actor,
            now,
        );
        if routed {
            aq.dispatch(Task::Lead { dest });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obligation_status_transitions_are_distinct() {
        assert_ne!(ObligationStatus::Pending, ObligationStatus::Delivered);
        assert_ne!(
            ObligationStatus::Pending,
            ObligationStatus::InTransit {
                courier: Entity::from_raw(0)
            }
        );
    }
}
