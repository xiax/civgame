use bevy::prelude::*;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use super::combat::{CombatTarget, Health, Body};
use super::faction::{FactionMember, FactionRegistry};
use super::goals::AgentGoal;
use super::lod::LodLevel;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;

const FOOD_STEAL_PER_TICK: f32 = 0.5;
const RAID_CANCEL_STOCK:  f32 = 5.0;
const RIVAL_HAS_FOOD:     f32 = 10.0;

/// Faction-level decision: set raid_target when food_stock == 0.
pub fn faction_decision_system(mut registry: ResMut<FactionRegistry>) {
    // Pass 1: collect decisions
    let snapshot: Vec<(u32, f32, (i16, i16))> = registry.factions
        .iter()
        .map(|(&id, f)| (id, f.food_stock, f.home_tile))
        .collect();

    let mut decisions: Vec<(u32, Option<u32>)> = Vec::new();

    for &(id, stock, home) in &snapshot {
        if stock > RAID_CANCEL_STOCK {
            decisions.push((id, None));
            continue;
        }
        if stock > 0.0 {
            continue; // not empty yet, keep current state
        }
        // Find nearest rival with food
        let rival = snapshot.iter()
            .filter(|&&(rid, rstock, _)| rid != id && rstock > RIVAL_HAS_FOOD)
            .min_by_key(|&&(_, _, rtile)| {
                let dx = (rtile.0 as i32 - home.0 as i32).abs();
                let dy = (rtile.1 as i32 - home.1 as i32).abs();
                dx + dy
            })
            .map(|&(rid, _, _)| rid);

        decisions.push((id, rival));
    }

    // Clear all under_raid flags
    for f in registry.factions.values_mut() {
        f.under_raid = false;
    }

    // Apply decisions
    for (id, target) in decisions {
        if let Some(f) = registry.factions.get_mut(&id) {
            f.raid_target = target;
        }
    }

    // Set under_raid on targets
    let targets: Vec<u32> = registry.factions.values()
        .filter_map(|f| f.raid_target)
        .collect();

    for target_id in targets {
        if let Some(f) = registry.factions.get_mut(&target_id) {
            f.under_raid = true;
        }
    }
}

/// Agents at enemy camp steal food and engage the nearest person there.
pub fn raid_execution_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut query: Query<(
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
) {
    let mut food_steals: Vec<(u32, f32)> = Vec::new();

    for (entity, mut ai, mut agent, mut combat_target, member, goal, transform, lod, slot) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) { continue; }
        if *goal != AgentGoal::Raid { continue; }

        let raid_target_faction = match registry.raid_target(member.faction_id) {
            Some(id) => id,
            None     => continue,
        };

        let enemy_home = match registry.home_tile(raid_target_faction) {
            Some(h) => h,
            None    => continue,
        };

        if ai.state != AiState::Working || ai.target_tile != enemy_home { continue; }

        // Steal food from enemy camp
        if registry.food_stock(raid_target_faction) >= 1.0 {
            food_steals.push((raid_target_faction, FOOD_STEAL_PER_TICK));
            agent.add_good(Good::Food, 1);
        }

        // Find a defender (enemy faction member) nearby to attack
        if combat_target.0.is_none() {
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            'find: for dy in -2..=2i32 {
                for dx in -2..=2i32 {
                    for &other in spatial.get(tx + dx, ty + dy) {
                        if other == entity { continue; }
                        if let Ok((other_fm, health, body)) = faction_query.get(other) {
                            if other_fm.faction_id == raid_target_faction {
                                let is_dead = match (health, body) {
                                    (Some(h), _) if h.is_dead() => true,
                                    (_, Some(b)) if b.is_dead() => true,
                                    _ => false,
                                };
                                if is_dead { continue; }

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

    for (faction_id, amount) in food_steals {
        if let Some(f) = registry.factions.get_mut(&faction_id) {
            f.food_stock = (f.food_stock - amount).max(0.0);
        }
    }
}
