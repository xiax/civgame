use crate::economy::agent::EconomicAgent;
use crate::economy::core_ids;
use crate::economy::item::Item;
use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::carry::Carrier;
use crate::simulation::excavation::{
    advance as excavation_advance, excavation_depth_cap, AdvanceOutcome, ExcavationKey,
    ExcavationMap, ExcavationMode, LEVEL_WORK_TICKS,
};
use crate::simulation::construction::WallMap;
use crate::simulation::faction::StorageTileMap;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::gather_claims::{release_gather_claim, GatherClaims};
use crate::simulation::goals::AgentGoal;
use crate::simulation::htn::{
    record_routing_failure, record_target_failure, MethodHistory, MethodOutcome,
};
use crate::simulation::items::GroundItem;
use crate::simulation::knowledge::DiscoveryActionEvent;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::MemoryKind;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::plants::{
    despawn_plant_internals, GrowthStage, PlantKind, PlantMap, PlantSpriteIndex,
};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::ActivityKind;
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::{TileCarvedEvent, TileChangedEvent};
use crate::world::globe::Globe;
use crate::world::terrain::{tile_to_world, world_to_tile, WorldGen};
use crate::world::tile::TileKind;
use bevy::ecs::system::SystemParam;
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

// ── P6b: stale-target neighbor-scan retarget ─────────────────────────────────

/// Cooldown between consecutive retargets on the same agent. Conservative:
/// one swap per chain (40 ticks ≈ 2s @ 20Hz) is enough to recover from a
/// just-harvested neighbor's plant without looping forever on a fully depleted
/// cluster.
pub const P6B_RETARGET_COOLDOWN: u64 = 40;
/// Chebyshev radius for the neighbor-scan. Mirrors the "agent stands inside
/// the field but doesn't see it" symptom — adjacent / 2-hop plants are the
/// realistic recovery candidates; farther afield, re-dispatch should pick a
/// fresh cluster.
pub const P6B_RETARGET_RADIUS: i32 = 2;

/// Find the closest unclaimed mature plant matching the agent's outstanding
/// `MemoryKind` claim within `radius` of `from`. Used by `gather_system`'s
/// stale-arrival branch (P6b). Returns the candidate tile and entity; a hit
/// is *not* atomic with the claim swap — caller must `release` then `add`
/// before mutating `Task::Gather`.
///
/// `kind`: the agent's `active_gather_claim.kind` — drives the
/// `MemoryKind → PlantKind` filter.  `AnyEdible` admits Grain | BerryBush;
/// `Resource(WOOD)` admits Tree; everything else returns `None`
/// (Resource(STONE) lives on the tile branch, not the plant branch).
fn retarget_neighbor(
    plant_map: &PlantMap,
    plant_query: &Query<(
        &mut crate::simulation::plants::Plant,
        Option<&crate::simulation::shared_knowledge::LandClaim>,
    )>,
    kind: MemoryKind,
    from: (i32, i32),
    radius: i32,
    viewer: Entity,
    now: u64,
    claims: &GatherClaims,
) -> Option<((i32, i32), Entity)> {
    use crate::simulation::plants::GrowthStage;
    let wood = MemoryKind::wood();
    let allows = |pk: PlantKind| -> bool {
        if kind == MemoryKind::AnyEdible {
            matches!(pk, PlantKind::Grain | PlantKind::BerryBush)
        } else if kind == wood {
            matches!(pk, PlantKind::Tree)
        } else {
            false
        }
    };
    let mut best: Option<((i32, i32), Entity, i32)> = None;
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            if dx == 0 && dy == 0 {
                continue;
            }
            let tile = (from.0 + dx, from.1 + dy);
            let Some(&entity) = plant_map.0.get(&tile) else {
                continue;
            };
            // Read-only get — caller will mutate if and only if it commits.
            let Ok((plant, _land_claim)) = plant_query.get(entity) else {
                continue;
            };
            if plant.stage != GrowthStage::Mature {
                continue;
            }
            if !allows(plant.kind) {
                continue;
            }
            if claims.pressure(tile, now, viewer) > 0 {
                continue;
            }
            let dist = dx.abs().max(dy.abs());
            match best {
                None => best = Some((tile, entity, dist)),
                Some((_, _, d)) if dist < d => best = Some((tile, entity, dist)),
                _ => {}
            }
        }
    }
    best.map(|(t, e, _)| (t, e))
}

// ── gather_system ─────────────────────────────────────────────────────────────

/// Routing resources bundled together so `gather_system` stays under Bevy's
/// 16-tuple `IntoSystem` ceiling after the 5c-ii-c-ii additions. `gather_system`
/// itself doesn't read these — only `finish_gather`, the chain-handoff helper.
#[derive(SystemParam)]
pub struct GatherRoutingResources<'w, 's> {
    pub storage_tile_map: Res<'w, StorageTileMap>,
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    /// Phase 5e-xi-c: read by `finish_gather` to look up `CraftOrder.anchor_tile`
    /// when the prefetch ring promotes a `Task::HaulToCraftOrder { order }`
    /// (the trailing leg of the harvest-grain-for-craft chain produced by
    /// `HarvestAndHaulGrainToCraftOrderMethod`).
    pub co_query: Query<'w, 's, &'static crate::simulation::crafting::CraftOrder>,
    /// Phase 5e-xiii-b: read by `finish_gather` to look up `Blueprint.tile`
    /// when the prefetch ring promotes a `Task::HaulToBlueprint { blueprint }`
    /// (the trailing leg of the gather-for-personal-build chain produced by
    /// `GatherAndHaulToPersonalBlueprintMethod`).
    pub bp_query: Query<'w, 's, &'static crate::simulation::construction::Blueprint>,
    /// Read by `finish_gather` to release any active gather claim staked at
    /// dispatch time. Mirrors the `release_reservation` plumbing for storage.
    pub gather_claims: Res<'w, GatherClaims>,
    /// Seasonal-farming jellyfish: per-tile nutrient/last-crop state for
    /// Grain harvest yield scaling + post-harvest debit. Bundled here so
    /// `gather_system` stays under Bevy's 16-param ceiling.
    pub field_tiles: ResMut<'w, crate::simulation::farm::FieldTileIndex>,
    pub calendar: Res<'w, crate::world::seasons::Calendar>,
    /// Draftwork v2: marker filter on Plant entities born inside a plot that
    /// was plowed this calendar year. Read by the Grain branch of the harvest
    /// path to apply the `PLOW_YIELD_MULT_*` bonus. Bundled here so
    /// `gather_system` stays under Bevy's 16-param ceiling.
    pub tilled_q: Query<'w, 's, (), With<crate::simulation::draftwork::Tilled>>,
    /// Seasonal-farming jellyfish: the Grain harvest branch credits an Autumn
    /// `FieldWork { phase: Harvest }` posting via `record_fieldwork_progress`
    /// when the harvester holds a `JobClaim::Farm` on it — without this,
    /// Autumn harvest postings never progress (harvest runs through
    /// `gather_system`, which had no `FieldWork` hook). Bundled here so
    /// `gather_system` stays under Bevy's 16-param ceiling.
    pub job_board: ResMut<'w, crate::simulation::jobs::JobBoard>,
    pub job_completed: EventWriter<'w, crate::simulation::jobs::JobCompletedEvent>,
    /// Incremental excavation: durable per-tile partial state shared with
    /// `dig_system`. Stone/ore branch consults + advances. Bundled here so
    /// `gather_system` stays under Bevy's 16-param ceiling.
    pub excavation_map: ResMut<'w, crate::simulation::excavation::ExcavationMap>,
    /// Emitted only at level-7 finalize; `aquifer_seep_emitter_system` reads it.
    pub tile_carved: EventWriter<'w, crate::world::chunk_streaming::TileCarvedEvent>,
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
///
/// `outcome` distinguishes the success path (yield is in hands; the prefetched
/// tail is valid — advance and route it) from the target-invalid path (plant
/// gone / wrong tile kind / no yield produced; the tail was predicated on yield
/// and must be dropped via `aq.cancel()` so the agent doesn't walk to storage
/// empty-handed).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FinishGatherOutcome {
    /// Yield was produced and is in hands/inventory; advance the chain.
    Completed,
    /// Target was invalid (despawned plant, de-matured plant, tile no longer a
    /// rock/plant). No yield was produced; abort the rest of the plan.
    TargetInvalid,
}

fn finish_gather(
    ai: &mut PersonAI,
    aq: &mut ActionQueue,
    actor: Entity,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    faction_id: Option<u32>,
    chunk_map: &ChunkMap,
    routing: &GatherRoutingResources,
    method_history: &mut MethodHistory,
    now: u64,
    outcome: FinishGatherOutcome,
) {
    // Drop any active gather claim before resetting AI state. Mirrors
    // `release_reservation` for storage: the claim was staked at dispatch
    // time by the four resource-target dispatchers, so every Gather exit
    // (success/fail/handoff) must release it or the cluster stays "claimed"
    // until expiry and downstream agents over-disperse.
    release_gather_claim(&routing.gather_claims, ai, actor);

    ai.state = AiState::Idle;
    ai.target_entity = None;
    ai.work_progress = 0;

    if outcome == FinishGatherOutcome::TargetInvalid {
        // The queued tail (Deposit / HaulToBlueprint / HaulToCraftOrder / Eat)
        // was predicated on this gather producing yield. Walking to storage
        // with empty hands is the visible bug — drop the chain wholesale.
        // `MethodHistory.FailedTarget` was recorded by the caller; the next
        // dispatcher tick will re-plan with that bias applied.
        aq.cancel();
        return;
    }

    aq.advance();

    // Chain handoff: route based on what the prefetch ring promoted.
    match aq.current {
        Task::DepositToFactionStorage {
            target_faction_id, ..
        } => {
            // `target_faction_id` overrides the actor's own faction (private
            // farm harvest routes to the household sub-faction's storage).
            let Some(fid) = target_faction_id.or(faction_id) else {
                // SOLO agent — no faction storage. The dispatcher already
                // filters SOLO out, so this is defensive.
                record_routing_failure(method_history, ai, now);
                aq.cancel();
                return;
            };
            let Some(storage_tile) = routing.storage_tile_map.nearest_for_faction(fid, cur_tile)
            else {
                // No storage tiles for this faction — drop the chain, hands
                // stay full. They'll be eligible to gather again next tick
                // (the legacy gather plan registry never had a "where do I
                // dump this" answer either; the agent just held the haul
                // cap until something else happened).
                record_routing_failure(method_history, ai, now);
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
                record_routing_failure(method_history, ai, now);
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
        }
        Task::HaulToBlueprint { blueprint } => {
            // Phase 5e-xiii-b: gather-for-personal-build chain trailing leg.
            // The `GatherAndHaulToPersonalBlueprintMethod` expansion is
            // `[Gather { tile }, HaulToBlueprint { blueprint }]`; once the
            // material is in hand, route to the bp's tile via
            // TaskKind::HaulMaterials (mirrors
            // `production::finish_withdraw_material`'s HaulToBlueprint arm).
            // Despawned/satisfied bps silently degrade to Idle.
            let Ok(bp) = routing.bp_query.get(blueprint) else {
                record_target_failure(method_history, ai, now);
                aq.cancel();
                return;
            };
            let bp_tile = (bp.tile.0, bp.tile.1);
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                bp_tile,
                TaskKind::HaulMaterials,
                Some(blueprint),
                &routing.chunk_graph,
                &routing.chunk_router,
                chunk_map,
                &routing.chunk_connectivity,
            );
            if !dispatched {
                record_routing_failure(method_history, ai, now);
                aq.cancel();
            }
        }
        Task::HaulToCraftOrder { order } => {
            // Phase 5e-xi-c: harvest-grain-for-craft chain trailing leg. The
            // `HarvestAndHaulGrainToCraftOrderMethod` expansion is
            // `[Gather { plant }, HaulToCraftOrder { order }]`; once the grain
            // is in hand, route to the order's anchor tile. Despawned/satisfied
            // orders silently degrade to Idle.
            let Ok(order_data) = routing.co_query.get(order) else {
                record_target_failure(method_history, ai, now);
                aq.cancel();
                return;
            };
            let dest = order_data.anchor_tile;
            let dispatched = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                dest,
                TaskKind::HaulToCraftOrder,
                Some(order),
                &routing.chunk_graph,
                &routing.chunk_router,
                chunk_map,
                &routing.chunk_connectivity,
            );
            if !dispatched {
                record_routing_failure(method_history, ai, now);
                aq.cancel();
            }
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
    mut shared: ResMut<crate::simulation::shared_knowledge::SharedKnowledge>,
    mut routing: GatherRoutingResources,
    mut sharecrop: crate::simulation::land::SharecropResources,
    mut plant_query: Query<(
        &mut crate::simulation::plants::Plant,
        Option<&crate::simulation::shared_knowledge::LandClaim>,
    )>,
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
        Option<&FactionMember>,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        &AgentGoal,
        &mut MethodHistory,
        Option<&crate::simulation::jobs::JobClaim>,
        Option<&crate::simulation::tools::ToolKit>,
    )>,
) {
    use crate::simulation::shared_knowledge::KnowledgeTier;
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
        faction_member,
        household_member,
        _goal,
        mut method_history,
        job_claim,
        toolkit,
    ) in agent_query.iter_mut()
    {
        // Resolve the finest tier the agent writes to — same rule as
        // `vision_system`. Depletion writes only to this tier; gossip
        // propagation handles settlement / faction visibility.
        let agent_tier = if let Some(hm) = household_member {
            KnowledgeTier::Household(hm.household_id)
        } else if let Some(fm) = faction_member {
            KnowledgeTier::Faction(fm.faction_id)
        } else {
            KnowledgeTier::Faction(0)
        };
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Gather as u16 {
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

            // P6b: stale-target neighbor-scan retarget. If the planned tile's
            // plant is despawned or immature, scan chebyshev≤2 for a same-kind
            // mature plant and atomically swap claim + Task::Gather { tile }.
            // Throttled to one retarget per chain via `last_retarget_tick`
            // (40-tick cooldown) so a depleted cluster doesn't loop forever.
            let stale = match plant_query.get(entity) {
                Err(_) => true,
                Ok((p, _)) if p.stage != GrowthStage::Mature => true,
                _ => false,
            };
            if stale {
                let cooldown_ok =
                    clock.tick.saturating_sub(ai.last_retarget_tick) >= P6B_RETARGET_COOLDOWN;
                if cooldown_ok {
                    if let Some((claim_tile, claim_kind)) = ai.active_gather_claim {
                        if let Some((new_tile, _new_entity)) = retarget_neighbor(
                            &plant_map,
                            &plant_query,
                            claim_kind,
                            cur_tile,
                            P6B_RETARGET_RADIUS,
                            actor,
                            clock.tick,
                            &routing.gather_claims,
                        ) {
                            // Re-route the agent to the new tile via the same
                            // helper the dispatcher uses (sets ai.state to
                            // Seeking/Routing so movement_system walks again,
                            // and on arrival flips back to Working). On a
                            // routing failure we fall through to the legacy
                            // FailedTarget path so the agent doesn't sit in
                            // limbo.
                            let dispatched = assign_task_with_routing(
                                &mut ai,
                                cur_tile,
                                cur_chunk,
                                new_tile,
                                TaskKind::Gather,
                                None,
                                &routing.chunk_graph,
                                &routing.chunk_router,
                                &chunk_map,
                                &routing.chunk_connectivity,
                            );
                            if dispatched {
                                routing.gather_claims.release(claim_tile, claim_kind, actor);
                                let expires = crate::simulation::gather_claims::suggested_expiry(
                                    clock.tick, cur_tile, new_tile,
                                );
                                routing
                                    .gather_claims
                                    .add(new_tile, claim_kind, actor, expires);
                                ai.active_gather_claim = Some((new_tile, claim_kind));
                                ai.last_retarget_tick = clock.tick;
                                ai.work_progress = 0;
                                aq.current = Task::Gather { tile: new_tile };
                                // Prune the now-stale cluster entry so subsequent
                                // dispatches don't re-pick it.
                                if !plant_map.0.contains_key(&(tx, ty)) {
                                    shared.report_depleted(agent_tier, (tx, ty), claim_kind);
                                }
                                continue;
                            }
                        }
                    }
                }

                // Fall through: no neighbor / cooldown / no active claim.
                // Original behavior — push FailedTarget + finish_gather.
                let stale_plant_kind = plant_query.get(entity).ok().map(|(p, _)| p.kind);
                if plant_query.get(entity).is_err() {
                    plant_map.0.remove(&(tx, ty));
                    shared.report_depleted(agent_tier, (tx, ty), MemoryKind::AnyEdible);
                    shared.report_depleted(agent_tier, (tx, ty), MemoryKind::wood());
                } else if let Some(k) = stale_plant_kind {
                    let kind = match k {
                        PlantKind::BerryBush | PlantKind::Grain => MemoryKind::AnyEdible,
                        PlantKind::Tree => MemoryKind::wood(),
                    };
                    shared.report_depleted(agent_tier, (tx, ty), kind);
                }
                if let Some(method_id) = ai.active_method.take() {
                    method_history.push(method_id, MethodOutcome::FailedTarget, clock.tick);
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    actor,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                    &mut method_history,
                    clock.tick,
                    FinishGatherOutcome::TargetInvalid,
                );
                continue;
            }
            // Reborrow as &mut after the immutable peek above.
            let (mut plant, land_claim) = plant_query.get_mut(entity).unwrap();

            let kind = plant.kind;
            // Spill owner: if the harvested plant carries a Household
            // `LandClaim`, any carrier-overflow spill at the harvest tile is
            // household-private (kitchen-garden harvest). Faction / Person /
            // Public claims yield public spill.
            let spill_owner_household: Option<u32> = land_claim.and_then(|lc| match lc.owner {
                crate::simulation::shared_knowledge::ResourceOwner::Household(h) => Some(h),
                _ => None,
            });

            // ── Realistic Tool Overhaul: per-plant tool gate ──────────────
            // Tree → Axe (Chop). Grain → Sickle (HarvestCrop). BerryBush is
            // hand-gatherable. A worker with NO `ToolKit` at all (fixture /
            // pre-tool agents) is treated as satisfied so the gate degrades
            // gracefully; an *empty* kit blocks/degrades.
            use crate::simulation::tools::{ToolRequirement, ToolUseKind};
            let plant_tool_req: Option<ToolRequirement> = match kind {
                PlantKind::Tree => Some(ToolRequirement::any(ToolUseKind::Chop)),
                PlantKind::Grain => Some(ToolRequirement::any(ToolUseKind::HarvestCrop)),
                PlantKind::BerryBush => None,
            };
            let has_tool_for_plant = plant_tool_req
                .map(|req| toolkit.map(|tk| tk.satisfies(&req)).unwrap_or(true))
                .unwrap_or(true);
            // Best matching tool tier → work-speed multiplier (faster tools
            // shorten the work threshold). No tool / no kit ⇒ Stone baseline.
            let tool_speed = plant_tool_req
                .and_then(|req| toolkit.and_then(|tk| tk.best_for(&req)))
                .map(|it| {
                    crate::simulation::tools::work_speed_mult(
                        crate::simulation::tools::item_tool_tier(it),
                    )
                })
                .unwrap_or(1.0);

            // Mature Grain with no Sickle: the worker simply cannot reap it.
            // Abort the gather (FailedTarget) — berries / loose items stay
            // hand-gatherable, but standing grain needs a sickle.
            if matches!(kind, PlantKind::Grain) && !has_tool_for_plant {
                if let Some(method_id) = ai.active_method.take() {
                    method_history.push(method_id, MethodOutcome::FailedTarget, clock.tick);
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    actor,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                    &mut method_history,
                    clock.tick,
                    FinishGatherOutcome::TargetInvalid,
                );
                continue;
            }
            // `has_tool` doubles tree-felling yield as before; the Axe gate
            // below additionally degrades a no-Axe fell to deadwood.
            let has_tool = agent.has_tool() && has_tool_for_plant;

            // A faster tool shortens the work threshold. `harvest_work_ticks`
            // is 0 for every plant today (gather pacing lives in movement);
            // the `clamp(0, 255)` keeps that 0 → instant-on-arrival behaviour
            // intact rather than forcing a spurious one-tick floor.
            let work_threshold = ((kind.harvest_work_ticks() as f32 / tool_speed).ceil() as i32)
                .clamp(0, 255) as u8;
            if ai.work_progress < work_threshold {
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
            // Seasonal-farming jellyfish: Grain yields scale with the tile's
            // live nutrient level (debited by `HARVEST_NUTRIENT_DEBIT` here).
            // Other crops keep `base_qty` (no field state).
            //
            // Draftwork v2: Grain harvested from a Plant entity that carries
            // the `Tilled` marker (planted into a plowed-this-year plot) gets
            // a 1.4× multiplier on the nutrient-tier base. The marker is set
            // by `production_system`'s Planter branch at planting time.
            let scaled_qty = if matches!(kind, crate::simulation::plants::PlantKind::Grain) {
                let nut = routing
                    .field_tiles
                    .by_tile
                    .get(&(tx, ty))
                    .map(|s| s.nutrients)
                    .unwrap_or(0);
                let base = crate::simulation::farm::grain_yield_for_nutrients(nut);
                if routing.tilled_q.get(entity).is_ok() {
                    crate::simulation::draftwork::apply_plow_yield_bonus(base)
                } else {
                    base
                }
            } else {
                base_qty
            };
            let qty = (scaled_qty as f32 * yield_mul).round().max(1.0) as u32;
            // Apply the per-tile nutrient debit + last-crop tag for Grain.
            if matches!(kind, crate::simulation::plants::PlantKind::Grain) {
                let cur_year = routing.calendar.year as u16;
                if let Some(state) = routing.field_tiles.by_tile.get_mut(&(tx, ty)) {
                    state.nutrients = state
                        .nutrients
                        .saturating_sub(crate::simulation::farm::HARVEST_NUTRIENT_DEBIT);
                    state.last_crop = Some(crate::simulation::plants::PlantKind::Grain);
                    state.last_worked_year = cur_year;
                }
            }
            let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());

            // Phase 6 sharecropping: if the harvested tile sits on a
            // `Tenure::Sharecropping` plot, the landlord's share lands
            // directly at their nearest faction storage tile and the
            // tenant routes only their cut. Outside sharecrop plots we
            // route the entire yield through the standard path.
            let tenant_qty = match crate::simulation::land::lookup_sharecrop_split(
                &sharecrop.plot_index,
                &sharecrop.plot_q,
                tx,
                ty,
                qty,
            ) {
                Some((tenant, landlord_share, landlord_faction)) => {
                    if let Some((sx, sy)) = routing
                        .storage_tile_map
                        .nearest_for_faction(landlord_faction, (agent_tx, agent_ty))
                    {
                        crate::simulation::items::spawn_or_merge_ground_item(
                            &mut commands,
                            &sharecrop.spatial,
                            &mut sharecrop.item_q,
                            sx,
                            sy,
                            yield_id,
                            landlord_share,
                        );
                    }
                    tenant
                }
                None => qty,
            };
            if tenant_qty > 0 {
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    yield_id,
                    tenant_qty,
                    agent_tx,
                    agent_ty,
                    spill_owner_household,
                );
            }

            for (extra_id, extra_qty) in kind.harvest_extra_yields() {
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    extra_id,
                    extra_qty,
                    agent_tx,
                    agent_ty,
                    spill_owner_household,
                );
            }

            let (skill, xp) = kind.harvest_skill_xp(has_tool);
            skills.gain_xp(skill, xp);

            for (drop_id, drop_qty) in kind.harvest_ground_drops(has_tool) {
                spawn_ground_drop(&mut commands, tx, ty, drop_id, drop_qty);
            }

            // Realistic Tool Overhaul: felling a Tree requires an Axe. With
            // no Axe the worker only collects fallen deadwood — the standing
            // tree is NOT despawned so it can be felled later with a real
            // axe. Berries / Grain keep their normal despawn rule.
            let tree_fell_blocked = matches!(kind, PlantKind::Tree) && !has_tool_for_plant;
            if kind.harvest_despawns(has_tool) && !tree_fell_blocked {
                despawn_plant_internals(
                    &mut commands,
                    entity,
                    (tx, ty),
                    &mut plant_map,
                    &mut plant_sprite_index,
                );
                // Depletion: a despawning harvest removes the resource from
                // the cluster's accounting so other agents stop routing here.
                let depleted_kind = match kind {
                    PlantKind::BerryBush | PlantKind::Grain => MemoryKind::AnyEdible,
                    PlantKind::Tree => MemoryKind::wood(),
                };
                shared.report_depleted(agent_tier, (tx, ty), depleted_kind);
            } else if tree_fell_blocked {
                // No-Axe deadwood collection — leave the tree standing and
                // Mature so it can be properly felled later. The cluster
                // entry is NOT depleted (the wood is still there).
            } else {
                plant.stage = GrowthStage::Harvested;
                plant.growth = 0;
                let depleted_kind = match plant.kind {
                    PlantKind::BerryBush | PlantKind::Grain => MemoryKind::AnyEdible,
                    PlantKind::Tree => MemoryKind::wood(),
                };
                shared.report_depleted(agent_tier, (tx, ty), depleted_kind);
            }

            // Seasonal-farming jellyfish: credit an Autumn `FieldWork`
            // posting's Harvest phase when this harvester holds a
            // `JobClaim::Farm` on it and the reaped tile is inside the
            // posting's area. `record_fieldwork_progress` no-ops unless the
            // backing posting is `FieldWork { phase: Harvest }`, so a
            // Prepare/Plant claim can't be cross-credited by a harvest.
            // Crop-agnostic: any farm-plantable kind (Grain, Berry, …) reaped
            // inside the claimed posting's area credits the posting — the
            // `planting_area_contains` rect guard already restricts credit to
            // the plot, so a wild berry forage can't cross-credit.
            if kind.is_farm_plantable() {
                if let Some(claim) = job_claim {
                    if matches!(claim.kind, crate::simulation::jobs::JobKind::Farm) {
                        let in_area = routing
                            .job_board
                            .get(claim.job_id)
                            .map(|p| {
                                crate::simulation::jobs::planting_area_contains(
                                    &p.progress,
                                    (tx, ty),
                                )
                            })
                            .unwrap_or(false);
                        if in_area {
                            crate::simulation::jobs::record_fieldwork_progress(
                                &mut commands,
                                &mut routing.job_board,
                                &mut routing.job_completed,
                                claim.job_id,
                                crate::simulation::farm::FarmWorkPhase::Harvest,
                                1,
                            );
                        }
                    }
                }
            }
        } else {
            // ── Tile harvest (stone / wall) ───────────────────────────────────

            let tile_kind = chunk_map.tile_kind_at(tx, ty);

            if matches!(
                tile_kind,
                Some(TileKind::Wall) | Some(TileKind::Stone) | Some(TileKind::Ore)
            ) {
                // ── Incremental excavation (7-level model).
                //
                // Per cycle: pay this level's yield (flat 1 unit + faction
                // multiplier), advance ExcavationMap by one. At level 7 the
                // tile finalises via carve::finalize_carved_tile.
                //
                // Bare-hands cap at HAND_DEPTH_LIMIT for stone-like material;
                // any Pick unlocks 7. Tier scales work-tick threshold.
                use crate::simulation::tools::{ToolRequirement, ToolUseKind};
                let pick_req = ToolRequirement::any(ToolUseKind::Mine);
                let pick_speed = toolkit
                    .and_then(|tk| tk.best_for(&pick_req))
                    .map(|it| {
                        crate::simulation::tools::work_speed_mult(
                            crate::simulation::tools::item_tool_tier(it),
                        )
                    })
                    .unwrap_or(1.0);
                let level_threshold = ((LEVEL_WORK_TICKS as f32 / pick_speed).ceil() as i32)
                    .clamp(1, 255) as u8;
                if ai.work_progress < level_threshold {
                    continue;
                }
                ai.work_progress = 0;

                let worked_z = ai.current_z as i32;
                let was_wall = tile_kind == Some(TileKind::Wall);
                let unwrapped_kind = tile_kind.unwrap_or(TileKind::Stone);

                // Per-cycle tool gate. A pick lost mid-excavation halts at
                // level 3 on stone-like material; soil-like is hand-diggable.
                let depth_cap = excavation_depth_cap(toolkit, unwrapped_kind);
                let key = ExcavationKey {
                    tile: (tx, ty),
                    z: worked_z as i8,
                    mode: ExcavationMode::Mine,
                };
                let current_level = routing.excavation_map.level_at(&key);
                if current_level >= depth_cap {
                    // No deeper progress possible — drop the chain. HTN can
                    // re-route to a tool acquisition or different site.
                    finish_gather(
                        &mut ai,
                        &mut aq,
                        actor,
                        cur_tile,
                        cur_chunk,
                        faction_id,
                        &chunk_map,
                        &routing,
                        &mut method_history,
                        clock.tick,
                        FinishGatherOutcome::TargetInvalid,
                    );
                    continue;
                }

                let mut yields = Vec::with_capacity(2);
                let outcome = excavation_advance(
                    &mut routing.excavation_map,
                    &mut chunk_map,
                    &gen,
                    &globe,
                    key,
                    &mut tile_changed,
                    &mut routing.tile_carved,
                    &mut yields,
                );

                let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
                let mut total_qty: u32 = 0;
                for (resource_id, qty) in yields {
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
                        None, // mining spills are public
                    );
                    if let Some(id) = faction_id {
                        if let Some(fd) = faction_registry.factions.get_mut(&id) {
                            fd.activity_log.increment(activity);
                        }
                    }
                    discovery_events.send(DiscoveryActionEvent { actor, activity });
                }
                let _ = total_qty; // kept for future debug instrumentation

                // Per-level XP (smaller than old STONE.xp=2 single-shot; sums
                // to ~7 across a fully-pickaxed carve).
                skills.gain_xp(SkillKind::Mining, 1);

                match outcome {
                    AdvanceOutcome::Levelled { new_level } => {
                        // Keep the task alive across partial levels. Only
                        // retire if the next step would exceed the tool cap
                        // or the carrier can't accept more stone.
                        let stone_item =
                            crate::economy::item::Item::new_commodity(
                                crate::economy::core_ids::stone(),
                            );
                        let next_blocked = new_level >= depth_cap
                            || carrier.should_return_to_deposit(stone_item);
                        if next_blocked {
                            finish_gather(
                                &mut ai,
                                &mut aq,
                                actor,
                                cur_tile,
                                cur_chunk,
                                faction_id,
                                &chunk_map,
                                &routing,
                                &mut method_history,
                                clock.tick,
                                FinishGatherOutcome::Completed,
                            );
                        }
                        // else: fall through, ai.state stays Working.
                        continue;
                    }
                    AdvanceOutcome::Carved => {
                        // Level 7 finalised — proceed to wall despawn + finish.
                    }
                }

                // For Mine the finalize opens floor at worked_z - 1 (see
                // ExcavationMode::Mine in excavation::advance). The wall
                // entity matches `ai.dest_tile`; remove only when column
                // has no solid tile at or above (the visible wall is gone).
                let target_floor_z = worked_z - 1;
                if was_wall && chunk_map.surface_z_at(tx, ty) < target_floor_z + 1 {
                    if let Some(wall_entity) = wall_map.0.remove(&ai.dest_tile) {
                        commands.entity(wall_entity).despawn_recursive();
                    }
                }

                finish_gather(
                    &mut ai,
                    &mut aq,
                    actor,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                    &mut method_history,
                    clock.tick,
                    FinishGatherOutcome::Completed,
                );
            } else if matches!(tile_kind, Some(TileKind::Marsh)) {
                // ── Phase F.2 — GatherReeds task variant. A `Task::Gather`
                // targeting a wetland Marsh tile harvests reeds: a fast,
                // tool-free, renewable bundle. The reed bed regrows under
                // marsh hydrology so the tile kind never changes.
                //
                // Per cycle: pay `REEDS_PER_GATHER` reeds after
                // `REEDS_WORK_TICKS` accumulated. Mirrors the stone
                // incremental-yield rhythm so a worker gathering reeds
                // for a chief `Stockpile{reeds}` posting fills hands at
                // similar pace.
                const REEDS_WORK_TICKS: u8 = 30;
                const REEDS_PER_GATHER: u32 = 2;
                if ai.work_progress < REEDS_WORK_TICKS {
                    continue;
                }
                ai.work_progress = 0;
                let reeds_id = *crate::economy::core_ids::Reeds
                    .get()
                    .expect("core_ids: reeds() called before init_core_ids()");
                let (agent_tx, agent_ty) = world_to_tile(transform.translation.truncate());
                let (_, _, mul) = faction_muls(
                    &mut faction_registry,
                    faction_id,
                    ActivityKind::Foraging,
                );
                let qty = (REEDS_PER_GATHER as f32 * mul).round().max(1.0) as u32;
                route_yield(
                    &mut commands,
                    &mut carrier,
                    &mut agent,
                    reeds_id,
                    qty,
                    agent_tx,
                    agent_ty,
                    None, // reeds gather spills are public
                );
                skills.gain_xp(SkillKind::Farming, 1);
                discovery_events.send(DiscoveryActionEvent {
                    actor,
                    activity: ActivityKind::Foraging,
                });
                // Keep the chain alive across cycles unless carrier full.
                let next_blocked = {
                    let reeds_item = crate::economy::item::Item::new_commodity(reeds_id);
                    carrier.should_return_to_deposit(reeds_item)
                };
                if next_blocked {
                    finish_gather(
                        &mut ai,
                        &mut aq,
                        actor,
                        cur_tile,
                        cur_chunk,
                        faction_id,
                        &chunk_map,
                        &routing,
                        &mut method_history,
                        clock.tick,
                        FinishGatherOutcome::Completed,
                    );
                }
            } else {
                // Not a stone/wall tile and not a plant -> target is invalid or already harvested
                shared.report_depleted(agent_tier, (tx, ty), MemoryKind::stone());
                shared.report_depleted(agent_tier, (tx, ty), MemoryKind::AnyEdible);
                shared.report_depleted(agent_tier, (tx, ty), MemoryKind::wood());
                if let Some(method_id) = ai.active_method.take() {
                    method_history.push(method_id, MethodOutcome::FailedTarget, clock.tick);
                }
                finish_gather(
                    &mut ai,
                    &mut aq,
                    actor,
                    cur_tile,
                    cur_chunk,
                    faction_id,
                    &chunk_map,
                    &routing,
                    &mut method_history,
                    clock.tick,
                    FinishGatherOutcome::TargetInvalid,
                );
            }
        }

        // ── Hands at haul cap → end gather step so the plan advances to deposit ──

        if carrier.should_return_to_deposit_held() {
            finish_gather(
                &mut ai,
                &mut aq,
                actor,
                cur_tile,
                cur_chunk,
                faction_id,
                &chunk_map,
                &routing,
                &mut method_history,
                clock.tick,
                FinishGatherOutcome::Completed,
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
    owner_household: Option<u32>,
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
                owner_household,
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

pub(crate) fn spawn_ground_drop(
    commands: &mut Commands,
    tx: i32,
    ty: i32,
    resource_id: ResourceId,
    qty: u32,
) {
    let (dx, dy) = adjacent_offset();
    let pos = tile_to_world(tx + dx, ty + dy);
    commands.spawn((
        GroundItem {
            item: Item::new_commodity(resource_id),
            qty,
            owner_household: None,
        },
        Transform::from_xyz(pos.x, pos.y, 0.3),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
        crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
        crate::simulation::obstacle::ConstructionObstacle {
            resolution: crate::simulation::obstacle::ObstacleResolution::Relocate,
        },
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
