use super::carry::Carrier;
use super::combat::{Body, BodyPart, CombatTarget, Health};
use super::faction::{
    FactionChief, FactionMember, FactionRegistry, RaidPhase, StorageReservations, StorageTileMap,
    SOLO,
};
use super::goals::AgentGoal;
use super::items::{Equipment, GroundItem};
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, Drafted, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::stats::Stats;
use super::tasks::{assign_task_with_routing, TaskKind};
use super::typed_task::{ActionQueue, Task, UNEMPLOYED_TASK_KIND};
use crate::economy::agent::EconomicAgent;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

// ── Raid tuning constants ──────────────────────────────────────────────────

/// Sustained crisis duration (both food deficit AND hunger crisis) required
/// before a faction commits to a raid.
pub const RAID_TRIGGER_DAYS: u32 = 2;
/// Minimum gap between raids — a faction can't re-raid until this elapses.
pub const RAID_COOLDOWN_DAYS: u32 = 10;
/// Average member hunger that counts as a hunger crisis.
pub const RAID_MIN_AVG_HUNGER: f32 = 140.0;
/// Per-member food stock at/above which the crisis is considered resolved
/// (aborts a `Preparing` raid; ends an `Engaged` one).
pub const RAID_CANCEL_FOOD_PER_MEMBER: f32 = 3.0;
/// Per-member food a raid target keeps in reserve — raiders won't drain a
/// faction below this, and a target this poor is never picked.
pub const RAID_RIVAL_RESERVE_PER_MEMBER: f32 = 5.0;
/// Raid party size = `min(RAID_MAX_PARTY_ABS, ceil(members * frac))`.
pub const RAID_MAX_PARTY_FRAC: f32 = 0.35;
pub const RAID_MAX_PARTY_ABS: usize = 8;
/// Per-raider cooldown between food steals.
pub const RAID_STEAL_COOLDOWN_TICKS: u32 = TICKS_PER_DAY / 8;
/// Maximum chebyshev distance to a viable raid target.
pub const RAID_MAX_TRAVEL_TILES: i32 = 500;
/// Hard timeout on the `Engaged` phase.
pub const RAID_ENGAGED_TIMEOUT_TICKS: u32 = TICKS_PER_DAY * 2;
/// Hard timeout on the `Preparing` phase.
pub const RAID_PREP_TIMEOUT_TICKS: u32 = TICKS_PER_DAY * 3;
/// A raider below this fraction of max HP is unfit for a raid party.
pub const RAID_CAPABILITY_MIN_HEALTH_FRAC: f32 = 0.5;
/// Below this many living party members, an in-flight raid ends.
pub const RAID_MIN_PARTY: usize = 2;

// ── Capability scoring ─────────────────────────────────────────────────────

/// Physical-capability score for raid-party selection. Returns `None` for an
/// unfit member (dead, destroyed torso, or below 50% HP); otherwise a score
/// in roughly `[0, 1]` blending HP, strength, and being armed.
pub fn raid_capability(
    health: Option<&Health>,
    body: Option<&Body>,
    stats: Option<&Stats>,
    armed: bool,
) -> Option<f32> {
    let health = health?;
    if health.is_dead() {
        return None;
    }
    let hp_frac = health.fraction();
    if hp_frac < RAID_CAPABILITY_MIN_HEALTH_FRAC {
        return None;
    }
    if let Some(b) = body {
        if b.is_dead() {
            return None;
        }
        let torso = b.parts[BodyPart::Torso as usize];
        if (torso.current as u32) * 2 < torso.max as u32 {
            return None;
        }
    }
    // 3d6 strength → roughly 0..1 (3 → 0, 18 → 1).
    let str_norm = stats
        .map(|s| ((s.strength as f32 - 3.0) / 15.0).clamp(0.0, 1.0))
        .unwrap_or(0.5);
    let armed_score = if armed { 1.0 } else { 0.0 };
    Some(0.5 * hp_frac + 0.3 * str_norm + 0.2 * armed_score)
}

/// True if the agent carries a weapon anywhere (equipped, hands, inventory).
fn agent_is_armed(
    equipment: Option<&Equipment>,
    carrier: Option<&Carrier>,
    agent: Option<&EconomicAgent>,
) -> bool {
    let weapon = crate::economy::core_ids::weapon();
    equipment.map(|e| e.has_resource(weapon)).unwrap_or(false)
        || carrier
            .map(|c| c.quantity_of_resource(weapon) > 0)
            .unwrap_or(false)
        || agent
            .map(|a| a.quantity_of_resource(weapon) > 0)
            .unwrap_or(false)
}

fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

// ── Target selection ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct FactionSnap {
    id: u32,
    home: (i32, i32),
    food: f32,
    members: u32,
}

/// Pick the nearest viable raid target for `attacker`, applying the full
/// exclusion filter (self / SOLO / kin / tributary relationships / food-poor /
/// too distant). Returns `None` when nothing qualifies.
fn pick_raid_target(
    registry: &FactionRegistry,
    attacker: u32,
    snaps: &[FactionSnap],
) -> Option<u32> {
    let attacker_data = registry.factions.get(&attacker)?;
    let attacker_home = attacker_data.home_tile;
    let attacker_root = registry.root_faction(attacker);

    let mut best: Option<(u32, i32)> = None;
    for snap in snaps {
        if snap.id == attacker || snap.id == SOLO {
            continue;
        }
        let Some(cand) = registry.factions.get(&snap.id) else {
            continue;
        };
        // Abstract world-map factions are raided via `world_sim`, not the
        // entity raid FSM — never target one with a marching party.
        if !cand.materialized {
            continue;
        }
        // Kin: same root faction (covers parent/child household nesting).
        if registry.root_faction(snap.id) == attacker_root {
            continue;
        }
        // Tributary relationship in either direction.
        if attacker_data.dominance_over.contains(&snap.id)
            || attacker_data.subordinate_to == Some(snap.id)
            || cand.dominance_over.contains(&attacker)
            || cand.subordinate_to == Some(attacker)
        {
            continue;
        }
        // Too poor to be worth raiding.
        if snap.food <= snap.members as f32 * RAID_RIVAL_RESERVE_PER_MEMBER {
            continue;
        }
        let dist = chebyshev(attacker_home, snap.home);
        if dist > RAID_MAX_TRAVEL_TILES {
            continue;
        }
        if best.map(|(_, d)| dist < d).unwrap_or(true) {
            best = Some((snap.id, dist));
        }
    }
    best.map(|(id, _)| id)
}

// ── Per-faction member capability scan ─────────────────────────────────────

struct MemberInfo {
    entity: Entity,
    capability: Option<f32>,
    is_chief: bool,
    armed: bool,
}

/// Select a raid party ranked by physical capability. Chief is excluded
/// unless the party would otherwise fall below `RAID_MIN_PARTY`.
fn select_raid_party(members: &[MemberInfo], faction_members: u32) -> Vec<Entity> {
    let cap = RAID_MAX_PARTY_ABS
        .min(((faction_members as f32 * RAID_MAX_PARTY_FRAC).ceil() as usize).max(1));

    let mut fit: Vec<(&MemberInfo, f32)> = members
        .iter()
        .filter(|m| !m.is_chief)
        .filter_map(|m| m.capability.map(|c| (m, c)))
        .collect();
    // Highest capability first; deterministic entity-id tie-break.
    fit.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.entity.cmp(&b.0.entity))
    });
    let mut party: Vec<Entity> = fit.iter().take(cap).map(|(m, _)| m.entity).collect();

    if party.len() < RAID_MIN_PARTY {
        // Top up with a capable chief if one exists.
        if let Some(chief) = members
            .iter()
            .find(|m| m.is_chief && m.capability.is_some())
        {
            if !party.contains(&chief.entity) {
                party.push(chief.entity);
            }
        }
    }
    party
}

// ── Faction-level raid decision / phase machine ────────────────────────────

/// Drives the per-faction `RaidPhase` state machine: sustained-pressure
/// trigger, party selection, preparation readiness, and end conditions.
/// Replaces the legacy single-tick `food == 0 && hunger >= 80` trigger.
pub fn faction_decision_system(
    mut registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    member_query: Query<(
        Entity,
        &FactionMember,
        &Needs,
        Option<&Health>,
        Option<&Body>,
        Option<&Stats>,
        Option<&Equipment>,
        Option<&Carrier>,
        Option<&EconomicAgent>,
        Has<FactionChief>,
    )>,
) {
    let now = clock.tick as u32;

    // Pass 1 — per-faction member scan: average hunger, living-entity set,
    // and capability-scored member list.
    let mut hunger_sum: AHashMap<u32, (f32, u32)> = AHashMap::default();
    let mut members_by_faction: AHashMap<u32, Vec<MemberInfo>> = AHashMap::default();
    let mut alive: AHashSet<Entity> = AHashSet::default();
    for (entity, member, needs, health, body, stats, equipment, carrier, agent, is_chief) in
        member_query.iter()
    {
        if member.faction_id == SOLO {
            continue;
        }
        alive.insert(entity);
        let e = hunger_sum.entry(member.faction_id).or_insert((0.0, 0));
        e.0 += needs.hunger;
        e.1 += 1;
        let armed = agent_is_armed(equipment, carrier, agent);
        let capability = raid_capability(health, body, stats, armed);
        members_by_faction
            .entry(member.faction_id)
            .or_default()
            .push(MemberInfo {
                entity,
                capability,
                is_chief,
                armed,
            });
    }

    // Snapshot for target selection.
    let snaps: Vec<FactionSnap> = registry
        .factions
        .iter()
        .map(|(&id, f)| FactionSnap {
            id,
            home: f.home_tile,
            food: f.storage.food_total(),
            members: f.member_count,
        })
        .collect();

    let faction_ids: Vec<u32> = registry.factions.keys().copied().collect();
    for id in faction_ids {
        if id == SOLO {
            continue;
        }
        // Abstract world-map factions run their raids through `world_sim`,
        // not this entity-faction FSM.
        if !registry.factions[&id].materialized {
            continue;
        }
        let (food, members) = {
            let f = &registry.factions[&id];
            (f.storage.food_total(), f.member_count.max(1) as f32)
        };
        let avg_hunger = hunger_sum
            .get(&id)
            .map(|&(sum, count)| if count > 0 { sum / count as f32 } else { 0.0 })
            .unwrap_or(0.0);

        // Streak bookkeeping (lazy-init to `now` so a fresh faction isn't
        // treated as having been in deficit since tick 0).
        {
            let f = registry.factions.get_mut(&id).unwrap();
            if f.food_deficit_streak_tick == 0 {
                f.food_deficit_streak_tick = now;
            }
            if f.hunger_crisis_streak_tick == 0 {
                f.hunger_crisis_streak_tick = now;
            }
            if food >= members {
                f.food_deficit_streak_tick = now;
            }
            if avg_hunger < RAID_MIN_AVG_HUNGER {
                f.hunger_crisis_streak_tick = now;
            }
        }

        let phase = registry.factions[&id].raid_phase;
        let new_phase: Option<RaidPhase> = match phase {
            RaidPhase::Idle => {
                let f = &registry.factions[&id];
                let deficit_dur = now.saturating_sub(f.food_deficit_streak_tick);
                let hunger_dur = now.saturating_sub(f.hunger_crisis_streak_tick);
                let threshold = RAID_TRIGGER_DAYS * TICKS_PER_DAY;
                if deficit_dur >= threshold && hunger_dur >= threshold {
                    if let Some(target) = pick_raid_target(&registry, id, &snaps) {
                        let empty = Vec::new();
                        let party = select_raid_party(
                            members_by_faction.get(&id).unwrap_or(&empty),
                            f.member_count,
                        );
                        if party.len() >= RAID_MIN_PARTY {
                            let f = registry.factions.get_mut(&id).unwrap();
                            f.raid_party = party;
                            f.raid_stolen_food = 0;
                            Some(RaidPhase::Preparing {
                                since_tick: now,
                                target,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            RaidPhase::Preparing { since_tick, target } => {
                let target_ok = raid_target_viable(&registry, id, target);
                let recovered = food >= members * RAID_CANCEL_FOOD_PER_MEMBER;
                let empty = Vec::new();
                let party_members: Vec<&MemberInfo> = members_by_faction
                    .get(&id)
                    .unwrap_or(&empty)
                    .iter()
                    .filter(|m| registry.factions[&id].raid_party.contains(&m.entity))
                    .collect();
                let living = party_members.len();
                let ready = party_members.iter().filter(|m| m.armed).count();
                let elapsed = now.saturating_sub(since_tick);

                if !target_ok || recovered || living < RAID_MIN_PARTY {
                    Some(end_to_cooldown(&mut registry, id, now, true))
                } else if elapsed >= RAID_PREP_TIMEOUT_TICKS && ready * 4 < living {
                    // Genuinely can't arm — give up.
                    Some(end_to_cooldown(&mut registry, id, now, true))
                } else if ready >= living
                    || (elapsed >= RAID_PREP_TIMEOUT_TICKS / 2 && ready * 2 >= living)
                {
                    Some(RaidPhase::Marching {
                        since_tick: now,
                        target,
                    })
                } else {
                    None
                }
            }
            RaidPhase::Marching { since_tick, target } => {
                let target_ok = raid_target_viable(&registry, id, target);
                let recovered = food >= members * RAID_CANCEL_FOOD_PER_MEMBER;
                let living = living_party_count(&registry, id, &alive);
                if !target_ok || recovered || living < RAID_MIN_PARTY {
                    Some(end_to_cooldown(&mut registry, id, now, false))
                } else {
                    // Marching → Engaged transition is owned by
                    // `raid_detection_system` (it has raider positions).
                    let _ = since_tick;
                    None
                }
            }
            RaidPhase::Engaged { since_tick, target } => {
                let target_food = registry.food_stock(target);
                let target_members = registry
                    .factions
                    .get(&target)
                    .map(|t| t.member_count.max(1) as f32)
                    .unwrap_or(0.0);
                let target_drained = target_food <= target_members * RAID_RIVAL_RESERVE_PER_MEMBER;
                let recovered = food >= members * RAID_CANCEL_FOOD_PER_MEMBER;
                let living = living_party_count(&registry, id, &alive);
                let timed_out = now.saturating_sub(since_tick) >= RAID_ENGAGED_TIMEOUT_TICKS;
                if recovered || target_drained || timed_out || living < RAID_MIN_PARTY {
                    Some(end_to_cooldown(&mut registry, id, now, false))
                } else {
                    None
                }
            }
            RaidPhase::Cooldown { until_tick } => {
                if now >= until_tick {
                    Some(RaidPhase::Idle)
                } else {
                    None
                }
            }
        };

        if let Some(np) = new_phase {
            let f = registry.factions.get_mut(&id).unwrap();
            f.raid_phase = np;
        }
        // Sync the derived `raid_target` projection.
        let f = registry.factions.get_mut(&id).unwrap();
        f.raid_target = f.raid_phase.target();
        if matches!(f.raid_phase, RaidPhase::Idle | RaidPhase::Cooldown { .. }) {
            f.raid_party.clear();
        }
    }
}

/// A target is still viable if it exists, isn't food-poor, and is in range.
fn raid_target_viable(registry: &FactionRegistry, attacker: u32, target: u32) -> bool {
    let Some(t) = registry.factions.get(&target) else {
        return false;
    };
    let Some(a) = registry.factions.get(&attacker) else {
        return false;
    };
    if t.storage.food_total() <= t.member_count as f32 * RAID_RIVAL_RESERVE_PER_MEMBER {
        return false;
    }
    chebyshev(a.home_tile, t.home_tile) <= RAID_MAX_TRAVEL_TILES
}

fn living_party_count(registry: &FactionRegistry, faction: u32, alive: &AHashSet<Entity>) -> usize {
    registry
        .factions
        .get(&faction)
        .map(|f| f.raid_party.iter().filter(|e| alive.contains(e)).count())
        .unwrap_or(0)
}

/// Transition a faction into `Cooldown`, clearing the raid party. `half_len`
/// applies a half-length cooldown for an aborted preparation.
fn end_to_cooldown(
    registry: &mut FactionRegistry,
    faction: u32,
    now: u32,
    half_len: bool,
) -> RaidPhase {
    let cooldown = RAID_COOLDOWN_DAYS * TICKS_PER_DAY / if half_len { 2 } else { 1 };
    if let Some(f) = registry.factions.get_mut(&faction) {
        if f.raid_stolen_food > 0 {
            info!(
                "Faction {} raid ended — stole {} food",
                faction, f.raid_stolen_food
            );
        }
        f.raid_stolen_food = 0;
        f.raid_party.clear();
    }
    RaidPhase::Cooldown {
        until_tick: now + cooldown,
    }
}

// ── Raid detection (Marching → Engaged + under_raid alarm) ─────────────────

/// Detects raiders near a faction's home: flips `Marching → Engaged` when a
/// raider reaches the enemy home, and sets the target's `under_raid` alarm.
pub fn raid_detection_system(
    mut registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    query: Query<(&FactionMember, &AgentGoal, &Transform)>,
) {
    let now = clock.tick as u32;
    for f in registry.factions.values_mut() {
        f.under_raid = false;
    }

    let mut reached_home: AHashSet<u32> = AHashSet::default();
    let mut near_home: AHashSet<u32> = AHashSet::default();

    for (member, goal, transform) in query.iter() {
        if *goal != AgentGoal::Raid {
            continue;
        }
        let target_faction_id = match registry.raid_target(member.faction_id) {
            Some(id) => id,
            None => continue,
        };
        let enemy_home = match registry.home_tile(target_faction_id) {
            Some(h) => h,
            None => continue,
        };
        let raider_tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        let dist = chebyshev(raider_tile, enemy_home);
        if dist <= 1 {
            reached_home.insert(member.faction_id);
        }
        if dist <= 30 {
            near_home.insert(target_faction_id);
        }
    }

    for tid in near_home {
        if let Some(f) = registry.factions.get_mut(&tid) {
            f.under_raid = true;
        }
    }

    // Marching → Engaged for any attacker whose raider reached the enemy home.
    for aid in reached_home {
        if let Some(f) = registry.factions.get_mut(&aid) {
            if let RaidPhase::Marching { target, .. } = f.raid_phase {
                f.raid_phase = RaidPhase::Engaged {
                    since_tick: now,
                    target,
                };
                f.raid_target = Some(target);
            }
        }
    }
}

// ── Raid preparation: arm party members before the march ───────────────────

/// Routes unarmed raid-party members during the `Preparing` phase to withdraw
/// and equip a weapon from faction storage. Mirrors the hunting-spear
/// dispatcher; the trailing `Equip` leg is primed by
/// `production::finish_withdraw_material`.
#[allow(clippy::too_many_arguments)]
pub fn raid_prep_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    storage_tile_map: Res<StorageTileMap>,
    storage_reservations: Res<StorageReservations>,
    registry: Res<FactionRegistry>,
    spatial: Res<SpatialIndex>,
    item_query: Query<&GroundItem>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut ActionQueue,
        &AgentGoal,
        &EconomicAgent,
        Option<&Carrier>,
        Option<&Equipment>,
        &Transform,
        &FactionMember,
        &LodLevel,
    )>,
) {
    let weapon_id = crate::economy::core_ids::weapon();
    for (entity, mut ai, mut aq, goal, agent, carrier, equipment, transform, member, lod) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant || *goal != AgentGoal::Raid {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        // Only party members of a faction still in the Preparing phase.
        let Some(faction) = registry.factions.get(&member.faction_id) else {
            continue;
        };
        if !matches!(faction.raid_phase, RaidPhase::Preparing { .. }) {
            continue;
        }
        if !faction.raid_party.contains(&entity) {
            continue;
        }
        if agent_is_armed(equipment, carrier, Some(agent)) {
            continue;
        }
        let stock = faction.storage.totals.get(&weapon_id).copied().unwrap_or(0);
        if stock == 0 {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        // Nearest faction storage tile with effective Weapon stock.
        let Some(tiles) = storage_tile_map.by_faction.get(&member.faction_id) else {
            continue;
        };
        let mut best_tile: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for &(tx, ty) in tiles {
            let mut tile_stock: u32 = 0;
            for &gi in spatial.get(tx, ty) {
                if let Ok(item) = item_query.get(gi) {
                    if item.item.resource_id == weapon_id && item.qty > 0 {
                        tile_stock = tile_stock.saturating_add(item.qty);
                    }
                }
            }
            let effective =
                tile_stock.saturating_sub(storage_reservations.get((tx, ty), weapon_id));
            if effective == 0 {
                continue;
            }
            let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
            if dist < best_dist {
                best_dist = dist;
                best_tile = Some((tx, ty));
            }
        }
        let Some(storage_tile) = best_tile else {
            continue;
        };

        let dispatched = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            storage_tile,
            TaskKind::WithdrawMaterial,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !dispatched {
            continue;
        }
        storage_reservations.add(storage_tile, weapon_id, 1);
        ai.reserved_tile = storage_tile;
        ai.reserved_resource = Some(weapon_id);
        ai.reserved_qty = 1;
        let _ = aq.dispatch(Task::WithdrawMaterial {
            resource_id: weapon_id,
            qty: 1,
                source_faction_id: None,
            });
        let _ = aq.enqueue(Task::Equip {
            slot: crate::simulation::items::EquipmentSlot::MainHand,
            resource_id: weapon_id,
        });
    }
}

// ── Raid execution: steal food + engage defenders ──────────────────────────

/// Raiders that reached the enemy home steal food (per-raider cooldown +
/// reserve gate) and engage the nearest defender.
#[allow(clippy::too_many_arguments)]
pub fn raid_execution_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    storage_tile_map: Res<StorageTileMap>,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut agents: Query<(
        Entity,
        &mut PersonAI,
        &mut EconomicAgent,
        &mut CombatTarget,
        &FactionMember,
        &AgentGoal,
        &Transform,
        &LodLevel,
        &BucketSlot,
    )>,
    faction_query: Query<(&FactionMember, Option<&Health>, Option<&Body>)>,
    mut ground_items: Query<&mut GroundItem>,
) {
    let now = clock.tick as u32;
    // (attacker_faction, target_faction) for each successful steal.
    let mut food_steals: Vec<(u32, u32)> = Vec::new();

    for (entity, mut ai, mut agent, mut combat_target, member, goal, transform, lod, slot) in
        agents.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *goal != AgentGoal::Raid {
            continue;
        }
        // Steal/fight only while Engaged.
        let phase = registry
            .factions
            .get(&member.faction_id)
            .map(|f| f.raid_phase)
            .unwrap_or(RaidPhase::Idle);
        if !matches!(phase, RaidPhase::Engaged { .. }) {
            continue;
        }
        let raid_target_faction = match phase.target() {
            Some(id) => id,
            None => continue,
        };
        let enemy_home = match registry.home_tile(raid_target_faction) {
            Some(h) => h,
            None => continue,
        };

        let raider_tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        if chebyshev(raider_tile, enemy_home) > 1 {
            continue;
        }

        // Steal food: per-raider cooldown + target reserve gate.
        let target_food = registry.food_stock(raid_target_faction);
        let target_members = registry
            .factions
            .get(&raid_target_faction)
            .map(|t| t.member_count as f32)
            .unwrap_or(0.0);
        let cooldown_ok = now.saturating_sub(ai.last_raid_steal_tick) >= RAID_STEAL_COOLDOWN_TICKS
            || ai.last_raid_steal_tick == 0;
        let reserve_ok = target_food - 1.0 >= target_members * RAID_RIVAL_RESERVE_PER_MEMBER;
        if target_food >= 1.0 && cooldown_ok && reserve_ok {
            food_steals.push((member.faction_id, raid_target_faction));
            agent.add_resource(crate::economy::core_ids::fruit(), 1);
            ai.last_raid_steal_tick = now;
        }

        // Engage the nearest enemy defender.
        if combat_target.0.is_none() {
            let (tx, ty) = raider_tile;
            'find: for dy in -2..=2i32 {
                for dx in -2..=2i32 {
                    for &other in spatial.get(tx + dx, ty + dy) {
                        if other == entity {
                            continue;
                        }
                        if let Ok((other_fm, health, body)) = faction_query.get(other) {
                            if other_fm.faction_id == raid_target_faction {
                                let is_dead = match (health, body) {
                                    (Some(h), _) if h.is_dead() => true,
                                    (_, Some(b)) if b.is_dead() => true,
                                    _ => false,
                                };
                                if is_dead {
                                    continue;
                                }
                                combat_target.0 = Some(other);
                                ai.state = AiState::Attacking;
                                break 'find;
                            }
                        }
                    }
                }
            }
        }
    }

    // Physically remove one food item per steal from the target's storage.
    for (attacker_faction, target_faction) in food_steals {
        if let Some(tiles) = storage_tile_map.by_faction.get(&target_faction) {
            'tile: for &(stx, sty) in tiles {
                for &gi_entity in spatial.get(stx, sty) {
                    if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                        if gi.item.resource_id.is_edible() && gi.qty > 0 {
                            if gi.qty == 1 {
                                commands.entity(gi_entity).despawn_recursive();
                            } else {
                                gi.qty -= 1;
                            }
                            break 'tile;
                        }
                    }
                }
            }
        }
        if let Some(f) = registry.factions.get_mut(&attacker_faction) {
            f.raid_stolen_food = f.raid_stolen_food.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_rejects_dead_and_low_hp() {
        let dead = Health {
            current: 0,
            max: 100,
        };
        assert!(raid_capability(Some(&dead), None, None, false).is_none());

        let weak = Health {
            current: 40,
            max: 100,
        };
        assert!(raid_capability(Some(&weak), None, None, false).is_none());

        let ok = Health {
            current: 80,
            max: 100,
        };
        assert!(raid_capability(Some(&ok), None, None, false).is_some());
    }

    #[test]
    fn capability_rewards_strength_and_arms() {
        let hp = Health {
            current: 100,
            max: 100,
        };
        let weak = Stats {
            strength: 3,
            dexterity: 10,
            constitution: 10,
            intelligence: 10,
            wisdom: 10,
            charisma: 10,
        };
        let strong = Stats {
            strength: 18,
            ..weak
        };
        let unarmed = raid_capability(Some(&hp), None, Some(&weak), false).unwrap();
        let armed_strong = raid_capability(Some(&hp), None, Some(&strong), true).unwrap();
        assert!(armed_strong > unarmed);
    }

    #[test]
    fn capability_rejects_destroyed_torso() {
        let hp = Health {
            current: 100,
            max: 100,
        };
        let mut body = Body::new_humanoid();
        body.parts[BodyPart::Torso as usize].current = 5; // < 50% of 40
        assert!(raid_capability(Some(&hp), Some(&body), None, false).is_none());
    }

    #[test]
    fn party_cap_honored() {
        let mk = |i: u64, cap: f32| MemberInfo {
            entity: Entity::from_raw(i as u32),
            capability: Some(cap),
            is_chief: false,
            armed: false,
        };
        let members: Vec<MemberInfo> = (0..10).map(|i| mk(i, 0.5 + i as f32 * 0.01)).collect();
        // 10 members, frac 0.35 → ceil(3.5) = 4, capped at 8 → 4.
        let party = select_raid_party(&members, 10);
        assert_eq!(party.len(), 4);
    }

    #[test]
    fn party_excludes_unfit_prefers_capable() {
        let members = vec![
            MemberInfo {
                entity: Entity::from_raw(1),
                capability: None,
                is_chief: false,
                armed: false,
            },
            MemberInfo {
                entity: Entity::from_raw(2),
                capability: Some(0.9),
                is_chief: false,
                armed: true,
            },
            MemberInfo {
                entity: Entity::from_raw(3),
                capability: Some(0.6),
                is_chief: false,
                armed: false,
            },
        ];
        let party = select_raid_party(&members, 6);
        // 6 members → ceil(2.1) = 3 cap, but only 2 fit.
        assert_eq!(party.len(), 2);
        assert_eq!(party[0], Entity::from_raw(2)); // most capable first
        assert!(!party.contains(&Entity::from_raw(1)));
    }

    #[test]
    fn party_tops_up_with_chief_when_short() {
        let members = vec![
            MemberInfo {
                entity: Entity::from_raw(1),
                capability: Some(0.7),
                is_chief: false,
                armed: false,
            },
            MemberInfo {
                entity: Entity::from_raw(2),
                capability: Some(0.9),
                is_chief: true,
                armed: false,
            },
        ];
        let party = select_raid_party(&members, 4);
        assert_eq!(party.len(), 2);
        assert!(party.contains(&Entity::from_raw(2))); // chief topped up
    }
}
