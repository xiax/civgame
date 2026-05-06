use crate::economy::agent::EconomicAgent;
use crate::economy::core_ids;
use crate::economy::item::Item;
use crate::economy::resource_catalog::ResourceId;
use crate::simulation::carry::Carrier;
use crate::simulation::carve::carve_tile;
use crate::simulation::construction::WallMap;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::goals::AgentGoal;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::{AgentMemory, MemoryKind};
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::plants::{GrowthStage, PlantKind, PlantMap, PlantSpriteIndex};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::faction::StorageTileMap;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use bevy::ecs::system::SystemParam;
use crate::simulation::knowledge::DiscoveryActionEvent;
use crate::simulation::technology::ActivityKind;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::Globe;
use crate::world::terrain::{tile_to_world, world_to_tile, WorldGen};
use crate::world::tile::TileKind;
use bevy::prelude::*;

// ── Stone / ore tile harvest profile ──────────────────────────────────────────
// Coal/Iron and the new ores (Copper/Tin/Gold/Silver) are no longer random
// rolls on Stone tiles — they're real Ore tiles produced by `proc_tile`'s
// stratification model. `carve_tile` returns the per-block (ResourceId, qty) drop.

struct StoneProfile {
    work_ticks: u8,
    base_yield_qty: u32,
    xp: u32,
}

const STONE: StoneProfile = StoneProfile {
    work_ticks: 30,
    base_yield_qty: 2,
    xp: 2,
};

/// Activity bucket to credit when a particular resource was just mined.
fn mining_activity(id: ResourceId) -> Option<ActivityKind> {
    let stone = *core_ids::Stone.get()?;
    let coal = *core_ids::Coal.get()?;
    let iron = *core_ids::Iron.get()?;
    let copper = *core_ids::Copper.get()?;
    let tin = *core_ids::Tin.get()?;
    let gold = *core_ids::Gold.get()?;
    let silver = *core_ids::Silver.get()?;
    if id == stone {
        Some(ActivityKind::StoneMining)
    } else if id == coal {
        Some(ActivityKind::CoalMining)
    } else if id == iron {
        Some(ActivityKind::IronMining)
    } else if id == copper {
        Some(ActivityKind::CopperMining)
    } else if id == tin {
        Some(ActivityKind::TinMining)
    } else if id == gold {
        Some(ActivityKind::GoldMining)
    } else if id == silver {
        Some(ActivityKind::SilverMining)
    } else {
        None
    }
}

// ── gather_system ─────────────────────────────────────────────────────────────

/// Routing resources bundled together so `gather_system` stays under Bevy's
/// 16-tuple `IntoSystem` ceiling after the 5c-ii-c-ii additions. `gather_system`
/// itself doesn't read these — only `finish_gather`, the chain-handoff helper.
#[derive(SystemParam)]
pub struct GatherRoutingResources<'w> {
    pub storage_tile_map: Res<'w, StorageTileMap>,
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
}

/// Phase 5c-ii-c-ii chain handoff: called by every `gather_system` exit path
/// (5 sites today) instead of inlining the legacy reset block. Performs the
/// standard Idle reset + `aq.advance()`, *and* if the prefetch ring promotes
/// a `Task::DepositToFactionStorage { .. }` into `current`, routes the agent
/// to the nearest faction storage tile and primes
/// `task_id = TaskKind::DepositResource` so `drop_items_at_destination_system`
/// picks up next tick.
///
/// The good payload on `Task::DepositToFactionStorage` is informational: the
/// deposit executor is parameterless (dumps everything in hand at the current
/// `dest_tile`), so the routing is identical regardless of the good. Carrying
/// it on the typed task lets a future inspector-side or chain-integrity check
/// assert "this chain expected to deposit Wood — did Gather actually leave
/// Wood in our hands?"
///
/// On routing failure (no faction storage, all storage unreachable, or SOLO
/// agent — though the dispatcher already gates SOLO out) the chain is dropped
/// via `aq.cancel()`. The agent stays Idle with full hands; the next dispatcher
/// tick will either re-dispatch a fresh chain (if memory still has a target)
/// or fall through to `Explore`.
fn finish_gather(
    ai: &mut PersonAI,
    aq: &mut ActionQueue,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    faction_id: Option<u32>,
    chunk_map: &ChunkMap,
    routing: &GatherRoutingResources,
) {
    ai.state = AiState::Idle;
    ai.task_id = PersonAI::UNEMPLOYED;
    ai.target_entity = None;
    ai.work_progress = 0;
    aq.advance();

    // Chain handoff: route based on what the prefetch ring promoted.
    match aq.current {
        Task::DepositToFactionStorage { .. } => {
            let Some(fid) = faction_id else {
                // SOLO agent — no faction storage. The dispatcher already
                // filters SOLO out, so this is defensive.
                aq.cancel();
                return;
            };
            let Some(storage_tile) =
                routing.storage_tile_map.nearest_for_faction(fid, cur_tile)
            else {
                // No storage tiles for this faction — drop the chain, hands
                // stay full. They'll be eligible to gather again next tick
                // (the legacy gather plan registry never had a "where do I
                // dump this" answer either; the agent just held the haul
                // cap until something else happened).
                aq.cancel();
                return;
            };
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                storage_tile,
                TaskKind::DepositResource,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                chunk_map,
                &routing.chunk_connectivity,
            );
            if !dispatched {
                aq.cancel();
            }
        }
        Task::Eat => {
            // Forage chain trailing leg under AcquireFood — eat in place.
            // Mirrors `production::finish_withdraw_food`'s Eat handoff: prime
            // the legacy channel directly so `eat_task_system` picks up next
            // tick. The Gather executor leaves harvested food in
            // hands/inventory; `eat_task_system` reads from both.
            ai.state = AiState::Working;
            ai.task_id = TaskKind::Eat as u16;
        }
        _ => {}
    }
}

pub fn gather_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut wall_map: ResMut<WallMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    mut discovery_events: EventWriter<DiscoveryActionEvent>,
    clock: Res<SimClock>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    mut faction_registry: ResMut<FactionRegistry>,
    routing: GatherRoutingResources,
    mut plant_query: Query<&mut crate::simulation::plants::Plant>,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        &Transform,
        Option<&mut AgentMemory>,
        Option<&mut ActivePlan>,
        Option<&FactionMember>,
        &AgentGoal,
    )>,
) {
    for (
        actor,
        mut ai,
        mut aq,
        mut agent,
        mut carrier,
        mut skills,
        slot,
        lod,
        transform,
        mut memory_opt,
        mut plan_opt,
        faction_member,
        _goal,
    ) in agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if ai.task_id != TaskKind::Gather as u16 {
            continue;
        }

        // Phase 3b-iv: tile comes from the typed `Task::Gather` variant. Falls
        // back to `dest_tile` for any unmigrated dispatcher; in steady state
        // the typed task agrees with `dest_tile` (both populated together).
        let (tx, ty) = aq
            .current
            .as_gather()
            .unwrap_or((ai.dest_tile.0 as i32, ai.dest_tile.1 as i32));

        let faction_id = faction_member
            .map(|fm| fm.faction_id)
            .filter(|&id| id != SOLO);

        // Agent's current tile + chunk for `finish_gather`'s routing decision
        // when the prefetch ring promotes a `DepositToFactionStorage` task.
        let cur_tile = world_to_tile(transform.translation.truncate());
        let cur_chunk = ChunkCoord(
            cur_tile.0.div_euclid(CHUNK_SIZE as i32),
            cur_tile.1.div_euclid(CHUNK_SIZE as i32),
        );

        if let Some(entity) = plant_map.0.get(&(tx, ty)).copied() {
            // ── Plant harvest ────────────────────────────────────────────────

            let Ok(mut plant) = plant_query.get_mut(entity) else {
                plant_map.0.remove(&(tx, ty));
                if let Some(ref mut mem) = memory_opt {
                    mem.forget((tx as i32, ty as i32), MemoryKind::AnyEdible);
                    mem.forget((tx as i32, ty as i32), MemoryKind::wood());
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                );
                continue;
            };
            if plant.stage != GrowthStage::Mature {
                if let Some(ref mut mem) = memory_opt {
                    let kind = match plant.kind {
                        PlantKind::BerryBush | PlantKind::Grain => MemoryKind::AnyEdible,
                        PlantKind::Tree => MemoryKind::wood(),
                    };
                    mem.forget((tx as i32, ty as i32), kind);
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                );
                continue;
            }

            let kind = plant.kind;
            let has_tool = agent.has_tool();

            if ai.work_progress < kind.harvest_work_ticks() {
                continue;
            }
            ai.work_progress = 0;

            // Faction multipliers & activity log
            let harvest_activity = kind.harvest_activity();
            let (food_mul, wood_mul, _) =
                faction_muls(&mut faction_registry, faction_id, harvest_activity);
            discovery_events.send(DiscoveryActionEvent {
                actor,
                activity: harvest_activity,
            });

            let (yield_id, base_qty) = kind.harvest_yield(has_tool);
            let wood_id = core_ids::wood();
            let is_edible = core_ids::catalog()
                .get(yield_id)
                .and_then(|d| d.edible_calories)
                .is_some();
            let yield_mul = if is_edible {
                food_mul
            } else if yield_id == wood_id {
                wood_mul
            } else {
                1.0
            };
            let qty = (base_qty as f32 * yield_mul).round().max(1.0) as u32;
            let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
            route_yield(
                &mut commands,
                &mut carrier,
                &mut agent,
                yield_id,
                qty,
                agent_tx,
                agent_ty,
            );

            for (extra_id, extra_qty) in kind.harvest_extra_yields() {
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    extra_id,
                    extra_qty,
                    agent_tx,
                    agent_ty,
                );
            }

            let (skill, xp) = kind.harvest_skill_xp(has_tool);
            skills.gain_xp(skill, xp);

            if let Some(ref mut mem) = memory_opt {
                mem.record((tx as i32, ty as i32), kind.harvest_memory_kind());
            }
            if let Some(ref mut plan) = plan_opt {
                plan.reward_acc += qty as f32 * kind.harvest_reward_per_unit();
            }

            for (drop_id, drop_qty) in kind.harvest_ground_drops(has_tool) {
                spawn_ground_drop(&mut commands, tx, ty, drop_id, drop_qty);
            }

            if kind.harvest_despawns(has_tool) {
                plant_map.0.remove(&(tx, ty));
                let cx = tx.div_euclid(CHUNK_SIZE as i32);
                let cy = ty.div_euclid(CHUNK_SIZE as i32);
                if let Some(vec) = plant_sprite_index.by_chunk.get_mut(&ChunkCoord(cx, cy)) {
                    vec.retain(|(e, _)| *e != entity);
                }
                commands.entity(entity).despawn_recursive();
            } else {
                plant.stage = GrowthStage::Harvested;
                plant.growth_ticks = 0;
            }
        } else {
            // ── Tile harvest (stone / wall) ───────────────────────────────────

            let tile_kind = chunk_map.tile_kind_at(tx, ty);

            if matches!(
                tile_kind,
                Some(TileKind::Wall) | Some(TileKind::Stone) | Some(TileKind::Ore)
            ) {
                // ── Mineable rock: Wall, Stone, or Ore.
                // Same carve operation; per-block (ResourceId, qty) drops come from
                // `carve_tile` which reads the actual material via tile_at_3d.
                if ai.work_progress < STONE.work_ticks {
                    continue;
                }
                ai.work_progress = 0;

                let target_floor_z = ai.current_z as i32;
                let was_wall = tile_kind == Some(TileKind::Wall);

                let drops = carve_tile(
                    &mut chunk_map,
                    &gen,
                    &globe,
                    tx,
                    ty,
                    target_floor_z,
                    &mut tile_changed,
                );

                let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
                let mut total_qty: u32 = 0;
                for (resource_id, qty) in drops {
                    if qty == 0 {
                        continue;
                    }
                    let activity =
                        mining_activity(resource_id).unwrap_or(ActivityKind::StoneMining);
                    let (_, _, mul) = faction_muls(&mut faction_registry, faction_id, activity);
                    let scaled = (qty as f32 * mul).round().max(1.0) as u32;
                    total_qty = total_qty.saturating_add(scaled);
                    route_yield(
                        &mut commands,
                        &mut carrier,
                        &mut agent,
                        resource_id,
                        scaled,
                        agent_tx,
                        agent_ty,
                    );
                    if let Some(id) = faction_id {
                        if let Some(fd) = faction_registry.factions.get_mut(&id) {
                            fd.activity_log.increment(activity);
                        }
                    }
                    discovery_events.send(DiscoveryActionEvent { actor, activity });
                }

                if total_qty == 0 {
                    // Carved a non-yielding tile (e.g. Dirt headspace); credit the
                    // baseline so XP/effort isn't entirely wasted.
                    total_qty = STONE.base_yield_qty;
                }

                skills.gain_xp(SkillKind::Mining, STONE.xp);

                // Despawn the Wall entity only if the column no longer has
                // any solid tile at or above the carved Z (the visible wall
                // is fully gone).
                if was_wall && chunk_map.surface_z_at(tx, ty) < target_floor_z + 1 {
                    if let Some(wall_entity) = wall_map.0.remove(&ai.dest_tile) {
                        commands.entity(wall_entity).despawn_recursive();
                    }
                }

                if let Some(ref mut mem) = memory_opt {
                    mem.record((tx as i32, ty as i32), MemoryKind::stone());
                }
                if let Some(ref mut plan) = plan_opt {
                    plan.reward_acc += total_qty as f32 * 0.3;
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                );
            } else {
                // Not a stone/wall tile and not a plant -> target is invalid or already harvested
                if let Some(ref mut mem) = memory_opt {
                    mem.forget((tx as i32, ty as i32), MemoryKind::stone());
                    mem.forget((tx as i32, ty as i32), MemoryKind::AnyEdible);
                    mem.forget((tx as i32, ty as i32), MemoryKind::wood());
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                );
            }
        }

        // ── Hands at haul cap → end gather step so the plan advances to deposit ──

        if carrier.is_at_haul_cap() {
            finish_gather(
                &mut ai,
                &mut aq,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
            );
        }
    }
}

/// Pick up `qty` of `good` into the carrier; spill any leftover at `(tx, ty)` as a GroundItem.
/// Light "personal" goods (Tools, Seeds when farmer-eligible) are not routed here — those
/// go through the inventory path during Scavenge or production. Gathering loads always go
/// to hands first.
fn route_yield(
    commands: &mut Commands,
    carrier: &mut Carrier,
    _agent: &mut EconomicAgent,
    resource_id: ResourceId,
    qty: u32,
    tx: i32,
    ty: i32,
) {
    if qty == 0 {
        return;
    }
    let item = Item::new_commodity(resource_id);
    let leftover = carrier.try_pick_up(item, qty);
    if leftover > 0 {
        let pos = tile_to_world(tx, ty);
        commands.spawn((
            GroundItem {
                item,
                qty: leftover,
            },
            Transform::from_xyz(pos.x, pos.y, 0.3),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
            crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
        ));
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn faction_muls(
    registry: &mut FactionRegistry,
    faction_id: Option<u32>,
    activity: ActivityKind,
) -> (f32, f32, f32) {
    if let Some(id) = faction_id {
        if let Some(fd) = registry.factions.get_mut(&id) {
            fd.activity_log.increment(activity);
            return (
                fd.food_yield_multiplier(),
                fd.wood_yield_multiplier(),
                fd.stone_yield_multiplier(),
            );
        }
    }
    (1.0, 1.0, 1.0)
}

fn spawn_ground_drop(commands: &mut Commands, tx: i32, ty: i32, resource_id: ResourceId, qty: u32) {
    let (dx, dy) = adjacent_offset();
    let pos = tile_to_world(tx + dx, ty + dy);
    commands.spawn((
        GroundItem {
            item: Item::new_commodity(resource_id),
            qty,
        },
        Transform::from_xyz(pos.x, pos.y, 0.3),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
    ));
}

fn adjacent_offset() -> (i32, i32) {
    match fastrand::u8(..4) {
        0 => (1, 0),
        1 => (-1, 0),
        2 => (0, 1),
        _ => (0, -1),
    }
}
