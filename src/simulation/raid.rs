use super::combat::{Body, CombatTarget, Health};
use super::faction::{FactionMember, FactionRegistry, StorageTileMap, SOLO};
use super::goals::AgentGoal;
use super::items::GroundItem;
use super::lod::LodLevel;
use super::needs::Needs;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use ahash::AHashMap;
use bevy::prelude::*;

const RAID_CANCEL_STOCK: f32 = 5.0;
const RIVAL_HAS_FOOD: f32 = 10.0;

/// Faction-level decision: set raid_target when food_stock == 0 and members are hungry.
pub fn faction_decision_system(
    mut registry: ResMut<FactionRegistry>,
    hunger_query: Query<(&FactionMember, &Needs)>,
) {
    // Calculate average hunger per faction
    let mut faction_hunger: AHashMap<u32, (f32, u32)> = AHashMap::default();
    for (member, needs) in hunger_query.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        let entry = faction_hunger.entry(member.faction_id).or_insert((0.0, 0));
        entry.0 += needs.hunger;
        entry.1 += 1;
    }

    // Pass 1: collect decisions
    let snapshot: Vec<(u32, f32, (i16, i16), f32)> = registry
        .factions
        .iter()
        .map(|(&id, f)| {
            let avg_hunger = faction_hunger
                .get(&id)
                .map(|&(sum, count)| if count > 0 { sum / count as f32 } else { 0.0 })
                .unwrap_or(0.0);
            (id, f.storage.food_total(), f.home_tile, avg_hunger)
        })
        .collect();

    let mut decisions: Vec<(u32, Option<u32>)> = Vec::new();

    for &(id, stock, home, avg_hunger) in &snapshot {
        if stock > RAID_CANCEL_STOCK {
            decisions.push((id, None));
            continue;
        }
        if stock > 0.0 {
            continue; // not empty yet, keep current state
        }

        // Desperation check: only raid if members are actually hungry (> 80 avg)
        if avg_hunger < 80.0 {
            decisions.push((id, None));
            continue;
        }

        // Find nearest rival with food
        let rival = snapshot
            .iter()
            .filter(|&&(rid, rstock, _, _)| rid != id && rstock > RIVAL_HAS_FOOD)
            .min_by_key(|&&(_, _, rtile, _)| {
                let dx = (rtile.0 as i32 - home.0 as i32).abs();
                let dy = (rtile.1 as i32 - home.1 as i32).abs();
                dx + dy
            })
            .map(|&(rid, _, _, _)| rid);

        decisions.push((id, rival));
    }

    // Apply decisions
    for (id, target) in decisions {
        if let Some(f) = registry.factions.get_mut(&id) {
            f.raid_target = target;
        }
    }
}

/// Detects if raiders are near a faction's home tile and sets under_raid flag.
pub fn raid_detection_system(
    mut registry: ResMut<FactionRegistry>,
    query: Query<(&FactionMember, &AgentGoal, &Transform)>,
) {
    // Clear all under_raid flags
    for f in registry.factions.values_mut() {
        f.under_raid = false;
    }

    // Set under_raid on targets ONLY if raiders are near
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

        let raider_pos = transform.translation.truncate();
        let home_pos = Vec2::new(
            enemy_home.0 as f32 * TILE_SIZE,
            enemy_home.1 as f32 * TILE_SIZE,
        );

        // If within 30 tiles of enemy home, trigger alarm
        if raider_pos.distance(home_pos) < 30.0 * TILE_SIZE {
            if let Some(f) = registry.factions.get_mut(&target_faction_id) {
                f.under_raid = true;
            }
        }
    }
}

/// Agents at enemy camp steal food and engage the nearest person there.
pub fn raid_execution_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    storage_tile_map: Res<StorageTileMap>,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
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
    // Collect (raided faction, GroundItem entity) pairs for deferred modification
    let mut food_steals: Vec<u32> = Vec::new();

    for (entity, mut ai, mut agent, mut combat_target, member, goal, transform, lod, slot) in
        agents.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if *goal != AgentGoal::Raid {
            continue;
        }

        let raid_target_faction = match registry.raid_target(member.faction_id) {
            Some(id) => id,
            None => continue,
        };

        let enemy_home = match registry.home_tile(raid_target_faction) {
            Some(h) => h,
            None => continue,
        };

        if ai.state != AiState::Working || ai.target_tile != enemy_home {
            continue;
        }

        // Steal food from enemy storage tiles
        if registry.food_stock(raid_target_faction) >= 1.0 {
            food_steals.push(raid_target_faction);
            agent.add_good(Good::Fruit, 1);
        }

        // Find a defender (enemy faction member) nearby to attack
        if combat_target.0.is_none() {
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
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

    // Physically remove 1 food item per steal from the enemy's storage tiles
    for faction_id in food_steals {
        let Some(tiles) = storage_tile_map.by_faction.get(&faction_id) else {
            continue;
        };
        'tile: for &(stx, sty) in tiles {
            for &gi_entity in spatial.get(stx as i32, sty as i32) {
                if let Ok(mut gi) = ground_items.get_mut(gi_entity) {
                    if gi.item.good.is_edible() && gi.qty > 0 {
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
}
