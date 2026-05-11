//! Player-command authority layer.
//!
//! Commit 1 (this file) — plumbing only, no behavior change. Coexists with the
//! legacy `PlayerOrder` path. Subsequent commits route the UI and HTN through
//! these types and retire `PlayerOrder` + the 28 scattered `Without<PlayerOrder>`
//! filters in favor of goal forcing.
//!
//! Design rationale lives in `~/.claude/plans/player-orders-and-drafting-resilient-key.md`.
//! Mirrors `MigrationTarget` (in `nomad.rs`) — sim-owned marker, dedicated
//! dispatcher, lifecycle system, preserve-arms in `goal_dispatch_system`.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::combat::CombatTarget;
use crate::simulation::construction::{Blueprint, BlueprintMap, BuildSiteKind};
use crate::simulation::faction::{FactionMember, SOLO};
use crate::simulation::items::TargetItem;
use crate::simulation::person::{PersonAI, PlayerOrder, PlayerOrderKind};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::TechId;
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

/// One UI-issued command targeting one or more actors. UI systems emit these;
/// the sim owns translation to typed tasks. UI never mutates `PersonAI` /
/// `ActionQueue` / `Commanded` directly.
///
/// Multi-actor commands (drag-select muster, group move) carry every actor in
/// `actors`; the sim attaches a `Commanded` to each.
#[derive(Event, Debug, Clone)]
pub struct PlayerCommandEvent {
    pub actors: Vec<Entity>,
    pub command: PlayerCommand,
}

/// What the player wants done. Variants embed all spatial / target data so the
/// dispatcher doesn't need to look anything up at translation time.
///
/// Adding a new order is a four-step change:
///   1. New variant here.
///   2. Match arm in `dispatch_player_command_system`.
///   3. Match arm in `player_command_lifecycle_system` (terminal-state check).
///   4. UI button emits the event.
#[derive(Debug, Clone)]
pub enum PlayerCommand {
    Move {
        tile: (i32, i32),
        z: i8,
    },
    Gather {
        tile: (i32, i32),
        z: i8,
    },
    Mine {
        tile: (i32, i32),
        z: i8,
    },
    Build {
        kind: BuildSiteKind,
        tile: (i32, i32),
        z: i8,
    },
    Deconstruct {
        tile: (i32, i32),
        z: i8,
    },
    DigDown {
        tile: (i32, i32),
        z: i8,
    },
    PickUpItem {
        item: Entity,
        tile: (i32, i32),
        z: i8,
    },
    PickUpCorpse {
        corpse: Entity,
        tile: (i32, i32),
        z: i8,
    },
    AttackEntity {
        foe: Entity,
        tile: (i32, i32),
        z: i8,
    },
    Teach {
        student: Entity,
        tile: (i32, i32),
        z: i8,
    },
    HoldLecture {
        tech: TechId,
    },
    ReadItem {
        tech: TechId,
    },
    EncodeTablet {
        tech: TechId,
    },
    /// Promote selection to military mode (`Drafted`). The dispatcher
    /// filters `actors` by `Profession::Hunter` + player faction.
    Muster,
    /// Group route order for already-drafted units. Empty actors list means
    /// "every drafted player-faction unit".
    MilitaryMove {
        tile: (i32, i32),
        z: i8,
    },
    MilitaryAttack {
        foe: Entity,
    },
}

/// Per-actor authority marker. Replaces `PlayerOrder` once Commit 3 lands.
///
/// Lifecycle: `Pending` (event drained, dispatch pending this tick) →
/// `Active` (routed, executing) → terminal (`Completed` / `Failed` /
/// `Superseded`). One tick after terminal, `reap_terminal_commands_system`
/// strips the component and the agent re-enters autonomy.
#[derive(Component, Debug)]
pub struct Commanded {
    pub command: PlayerCommand,
    pub status: CommandStatus,
    pub issued_tick: u32,
    /// Monotonic id stamped at drain. UI can match HUD feedback to issuance,
    /// and supersession can identify "is this the same order I sent?".
    pub command_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    Pending,
    Active,
    Completed,
    Failed(CommandFailure),
    Superseded,
}

impl CommandStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CommandStatus::Completed | CommandStatus::Failed(_) | CommandStatus::Superseded
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandFailure {
    /// Routing rejected the target — no reachable adjacent tile, or the
    /// target sits in a different connectivity component than the actor.
    Unreachable,
    /// The target entity (item / corpse / foe / blueprint) was already gone
    /// when the dispatcher attempted to act.
    TargetGone,
    /// The actor isn't eligible for this command kind (e.g. `Muster` on a
    /// non-Hunter; `Teach` with no teachable techs).
    Ineligible,
}

/// Monotonic command id source. Wraps at u32::MAX (~4B commands — never).
#[derive(Resource, Default)]
pub struct PlayerCommandIdGen {
    pub next: u32,
}

impl PlayerCommandIdGen {
    pub fn allocate(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }
}

/// Drain `PlayerCommandEvent`s and stamp `Commanded { status: Pending }` on
/// every actor. Supersedes any prior `Commanded` on the same actor (status
/// flips to `Superseded`; `reap_terminal_commands_system` strips it next tick).
///
/// Runs in `SimulationSet::Input` so the dispatcher (ParallelB) sees fresh
/// `Pending` markers the same FixedUpdate tick the event was emitted in.
pub fn drain_player_command_events_system(
    mut commands: Commands,
    mut reader: EventReader<PlayerCommandEvent>,
    clock: Res<crate::simulation::schedule::SimClock>,
    mut id_gen: ResMut<PlayerCommandIdGen>,
    mut existing: Query<&mut Commanded>,
) {
    let now = clock.tick as u32;
    for ev in reader.read() {
        for &actor in &ev.actors {
            // Mark any existing command on this actor as superseded so the
            // reap pass strips it next tick. The dispatcher won't act on a
            // superseded command; the new `Pending` (inserted below) wins.
            if let Ok(mut prior) = existing.get_mut(actor) {
                if !prior.status.is_terminal() {
                    prior.status = CommandStatus::Superseded;
                }
            }
            let id = id_gen.allocate();
            commands.entity(actor).insert(Commanded {
                command: ev.command.clone(),
                status: CommandStatus::Pending,
                issued_tick: now,
                command_id: id,
            });
        }
    }
}

/// Removes `Commanded` (and the legacy `PlayerOrder` marker) one tick after
/// `Commanded` reaches terminal status. UI consumers read
/// `RemovedComponents<Commanded>` for HUD feedback ("Order complete" /
/// "Couldn't reach target").
///
/// Runs in `SimulationSet::Sequential`, late, so executors have a full tick
/// at terminal status.
pub fn reap_terminal_commands_system(
    mut commands: Commands,
    query: Query<(Entity, &Commanded)>,
) {
    for (entity, c) in query.iter() {
        if c.status.is_terminal() {
            commands
                .entity(entity)
                .remove::<Commanded>()
                .remove::<PlayerOrder>();
        }
    }
}

/// Per-kind terminal-state detector. Runs after executors in Sequential. The
/// heuristic completion we used to live with (`state == Idle && task_id ==
/// UNEMPLOYED`) is replaced by explicit per-kind detection — Move arrival is
/// distinct from Gather completion is distinct from Build done. Each variant's
/// terminal condition lives in one place.
///
/// Adding a new order's completion = add a match arm here.
pub fn player_command_lifecycle_system(
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
    plant_query: Query<&crate::simulation::plants::Plant>,
    item_query: Query<(), With<crate::simulation::items::GroundItem>>,
    corpse_query: Query<&crate::simulation::corpse::Corpse>,
    health_query: Query<&crate::simulation::combat::Health>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    mut q: Query<(Entity, &mut Commanded, &PersonAI, &Transform)>,
) {
    for (entity, mut cmd, ai, transform) in q.iter_mut() {
        if cmd.status != CommandStatus::Active {
            continue;
        }
        let _ = entity;
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur = (cur_tx, cur_ty);

        let outcome = match cmd.command {
            PlayerCommand::Move { tile, .. } => {
                // Arrival within chebyshev 0 = same tile.
                if chebyshev(cur, tile) <= 0 {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::Gather { tile, .. } => completion_when_gather_target_gone(
                tile,
                ai,
                &plant_query,
                &chunk_map,
            ),
            PlayerCommand::Mine { tile, .. } => {
                // Walls turn into floor / loose rock when mined. The mine
                // target is done when the tile is no longer a Wall/Stone.
                use crate::world::tile::TileKind;
                let kind = chunk_map.tile_kind_at(tile.0, tile.1);
                if !matches!(kind, Some(TileKind::Wall) | Some(TileKind::Stone)) {
                    Some(CommandStatus::Completed)
                } else {
                    completion_when_agent_idle(ai)
                }
            }
            PlayerCommand::DigDown { tile, .. } => {
                // DigDown creates a Dirt floor (cave carved out). When the
                // dig finishes, the executor sets the agent Idle+UNEMPLOYED.
                let _ = tile;
                completion_when_agent_idle(ai)
            }
            PlayerCommand::Deconstruct { tile, .. } => {
                // Deconstruction completes when the structure at the tile is
                // gone (StructureIndex entry would be removed). Fall back to
                // idle check.
                let _ = tile;
                completion_when_agent_idle(ai)
            }
            PlayerCommand::Build { tile, .. } => {
                // Build completes when the blueprint at this tile is gone
                // (construction_system despawns it on completion).
                match bp_map.0.get(&tile) {
                    None => Some(CommandStatus::Completed),
                    Some(&bp_e) => {
                        if bp_query.get(bp_e).is_err() {
                            Some(CommandStatus::Completed)
                        } else {
                            None
                        }
                    }
                }
            }
            PlayerCommand::PickUpItem { item, .. } => {
                if item_query.get(item).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::PickUpCorpse { corpse, .. } => {
                if corpse_query.get(corpse).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::AttackEntity { foe, .. } => {
                if health_query.get(foe).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::Teach { .. }
            | PlayerCommand::HoldLecture { .. }
            | PlayerCommand::ReadItem { .. }
            | PlayerCommand::EncodeTablet { .. } => {
                // Knowledge orders have their own teardown via dedicated
                // teaching/encoding systems that strip the legacy
                // `PlayerOrder` marker. We mirror by completing when the
                // agent is idle and unemployed (the legacy completion rule).
                completion_when_agent_idle(ai)
            }
            PlayerCommand::Muster
            | PlayerCommand::MilitaryMove { .. }
            | PlayerCommand::MilitaryAttack { .. } => {
                // Military commands have indefinite lifetime — they last
                // until the player unmusters or issues a fresh command.
                // Don't auto-complete.
                None
            }
        };
        if let Some(new_status) = outcome {
            cmd.status = new_status;
        }
    }
}

fn completion_when_agent_idle(ai: &PersonAI) -> Option<CommandStatus> {
    if ai.state == crate::simulation::person::AiState::Idle
        && ai.task_id == PersonAI::UNEMPLOYED
    {
        Some(CommandStatus::Completed)
    } else {
        None
    }
}

fn completion_when_gather_target_gone(
    tile: (i32, i32),
    ai: &PersonAI,
    plant_query: &Query<&crate::simulation::plants::Plant>,
    chunk_map: &crate::world::chunk::ChunkMap,
) -> Option<CommandStatus> {
    use crate::world::tile::TileKind;
    // If the agent had a target_entity (plant) and it's gone, completed.
    if let Some(ent) = ai.target_entity {
        if plant_query.get(ent).is_err() {
            return Some(CommandStatus::Completed);
        }
    }
    // If the tile is no longer a gatherable kind, completed.
    let kind = chunk_map.tile_kind_at(tile.0, tile.1);
    if matches!(kind, Some(TileKind::Wall) | Some(TileKind::Stone)) {
        // Mining-like gather: completed when no longer Wall/Stone.
        return None;
    }
    completion_when_agent_idle(ai)
}

fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Routing resources bundled together (Bevy 16-param ceiling).
#[derive(SystemParam)]
pub struct CommandRouting<'w, 's> {
    pub chunk_map: Res<'w, ChunkMap>,
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub bp_map: ResMut<'w, BlueprintMap>,
    #[system_param(ignore)]
    pub _marker: std::marker::PhantomData<&'s ()>,
}

/// Translate a `PlayerCommand` to the equivalent legacy `PlayerOrder` so HTN
/// dispatchers' `Without<PlayerOrder>` filters still gate the actor. Removed
/// in Commit 3 along with the filters themselves.
fn to_legacy_player_order(command: &PlayerCommand) -> Option<PlayerOrder> {
    let (order, target_tile, target_z) = match *command {
        PlayerCommand::Move { tile, z } => (PlayerOrderKind::Move, tile, z),
        PlayerCommand::Gather { tile, z } => (PlayerOrderKind::Gather, tile, z),
        PlayerCommand::Mine { tile, z } => (PlayerOrderKind::Mine, tile, z),
        PlayerCommand::Build { kind, tile, z } => (PlayerOrderKind::Build(kind), tile, z),
        PlayerCommand::Deconstruct { tile, z } => (PlayerOrderKind::Deconstruct, tile, z),
        PlayerCommand::DigDown { tile, z } => (PlayerOrderKind::DigDown, tile, z),
        PlayerCommand::PickUpItem { item, tile, z } => {
            (PlayerOrderKind::PickUpItem(item), tile, z)
        }
        PlayerCommand::PickUpCorpse { corpse, tile, z } => {
            (PlayerOrderKind::PickUpCorpse(corpse), tile, z)
        }
        PlayerCommand::AttackEntity { foe, tile, z } => {
            (PlayerOrderKind::AttackEntity(foe), tile, z)
        }
        PlayerCommand::Teach { student, tile, z } => (PlayerOrderKind::Teach(student), tile, z),
        PlayerCommand::HoldLecture { tech } => (PlayerOrderKind::HoldLecture(tech), (0, 0), 0),
        PlayerCommand::ReadItem { tech } => (PlayerOrderKind::ReadItem(tech), (0, 0), 0),
        PlayerCommand::EncodeTablet { tech } => (PlayerOrderKind::EncodeTablet(tech), (0, 0), 0),
        // Muster / Military variants don't map to PlayerOrder — they live on
        // their own marker (`Drafted`) and don't gate HTN via `Without<PlayerOrder>`.
        PlayerCommand::Muster
        | PlayerCommand::MilitaryMove { .. }
        | PlayerCommand::MilitaryAttack { .. } => return None,
    };
    Some(PlayerOrder {
        order,
        target_tile,
        target_z,
    })
}

/// Dispatch every `Commanded { status: Pending }`. Translates the kind into
/// the same task chain the legacy `right_click_context_menu_system` used to
/// emit inline. Sets `status = Active` on success, `status = Failed(...)` on
/// routing failure.
///
/// Runs in `SimulationSet::ParallelB` after the drain system in `Input` so
/// fresh `Pending` markers route the same tick. Single source of truth for
/// player-command translation; UI never mutates AI state directly.
pub fn dispatch_player_command_system(
    mut commands: Commands,
    mut routing: CommandRouting,
    mut actors: Query<(
        Entity,
        &mut Commanded,
        &mut PersonAI,
        &mut ActionQueue,
        &Transform,
        Option<&FactionMember>,
    )>,
    mut target_item_q: Query<&mut TargetItem>,
    mut combat_target_q: Query<&mut CombatTarget>,
) {
    for (actor, mut cmd, mut ai, mut aq, transform, member) in actors.iter_mut() {
        if cmd.status != CommandStatus::Pending {
            continue;
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let faction_id = member.map(|m| m.faction_id).unwrap_or(SOLO);

        // Player commands are external preempts. Drop any prior typed-task
        // chain so the new task promotes immediately.
        aq.cancel();

        let outcome = dispatch_one(
            actor,
            &cmd.command,
            (cur_tx, cur_ty),
            cur_chunk,
            faction_id,
            &mut ai,
            &mut aq,
            &mut routing,
            &mut target_item_q,
            &mut combat_target_q,
            &mut commands,
        );

        match outcome {
            DispatchOutcome::Active => {
                cmd.status = CommandStatus::Active;
                if let Some(po) = to_legacy_player_order(&cmd.command) {
                    commands.entity(actor).insert(po);
                }
            }
            DispatchOutcome::Failed(reason) => {
                cmd.status = CommandStatus::Failed(reason);
            }
            DispatchOutcome::DeferredToLegacy => {
                // Knowledge commands (ReadItem) route through their own
                // existing systems via the inserted PlayerOrder marker.
                cmd.status = CommandStatus::Active;
                if let Some(po) = to_legacy_player_order(&cmd.command) {
                    commands.entity(actor).insert(po);
                }
            }
        }
    }
}

enum DispatchOutcome {
    Active,
    Failed(CommandFailure),
    /// Successfully attached the legacy marker, leaving the dispatch to the
    /// pre-existing handler (used for knowledge orders whose systems already
    /// own the lifecycle).
    DeferredToLegacy,
}

#[allow(clippy::too_many_arguments)]
fn dispatch_one(
    actor: Entity,
    command: &PlayerCommand,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    faction_id: u32,
    ai: &mut PersonAI,
    aq: &mut ActionQueue,
    routing: &mut CommandRouting,
    target_item_q: &mut Query<&mut TargetItem>,
    combat_target_q: &mut Query<&mut CombatTarget>,
    commands: &mut Commands,
) -> DispatchOutcome {
    use PlayerCommand::*;
    match *command {
        Move { tile, z } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Idle,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            ai.target_z = z;
            DispatchOutcome::Active
        }
        Gather { tile, .. } | Mine { tile, .. } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Gather,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::Gather { tile });
            DispatchOutcome::Active
        }
        DigDown { tile, .. } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Dig,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::Dig { tile });
            DispatchOutcome::Active
        }
        Deconstruct { tile, .. } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Deconstruct,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            DispatchOutcome::Active
        }
        PickUpItem { item, tile, .. } => {
            if commands.get_entity(item).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Scavenge,
                Some(item),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            if let Ok(mut ti) = target_item_q.get_mut(actor) {
                ti.0 = Some(item);
            }
            aq.dispatch(Task::Scavenge { target: item });
            DispatchOutcome::Active
        }
        PickUpCorpse { corpse, tile, .. } => {
            if commands.get_entity(corpse).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::PickUpCorpse,
                Some(corpse),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::PickUpCorpse { corpse });
            DispatchOutcome::Active
        }
        AttackEntity { foe, tile, .. } => {
            if commands.get_entity(foe).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::MilitaryAttack,
                Some(foe),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            if let Ok(mut ct) = combat_target_q.get_mut(actor) {
                ct.0 = None;
            }
            DispatchOutcome::Active
        }
        Build { kind, tile, z: _z } => {
            // Spawn the personal blueprint if there isn't one at this tile.
            let bp_entity = if let Some(&e) = routing.bp_map.0.get(&tile) {
                Some(e)
            } else {
                let wp = tile_to_world(tile.0, tile.1);
                let bz = routing.chunk_map.surface_z_at(tile.0, tile.1) as i8;
                let bp_e = commands
                    .spawn((
                        Blueprint::new(faction_id, Some(actor), kind, tile, bz),
                        Transform::from_xyz(wp.x, wp.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                routing.bp_map.0.insert(tile, bp_e);
                Some(bp_e)
            };
            let task_kind = if matches!(kind, BuildSiteKind::Bed) {
                TaskKind::ConstructBed
            } else {
                TaskKind::Construct
            };
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                task_kind,
                bp_entity,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            if let Some(bp) = bp_entity {
                aq.dispatch(Task::Construct { blueprint: bp });
            }
            DispatchOutcome::Active
        }
        Teach { student, tile, .. } => {
            if commands.get_entity(student).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Teach,
                Some(student),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            // Adjacency markers (`TeachingPair` / `BeingTaught`) are inserted
            // by `apply_teach_order_system` once the teacher arrives.
            DispatchOutcome::DeferredToLegacy
        }
        HoldLecture { .. } | ReadItem { .. } | EncodeTablet { .. } => {
            // Knowledge orders route through dedicated systems in `teaching.rs`
            // and `jobs.rs` keyed off the legacy `PlayerOrder` marker. The
            // dispatcher attaches that marker; the specialized systems handle
            // the rest.
            DispatchOutcome::DeferredToLegacy
        }
        Muster | MilitaryMove { .. } | MilitaryAttack { .. } => {
            // Military commands are handled by a sibling dispatcher (Commit 2
            // follow-on); for now we leave the existing muster + military
            // right-click paths in place and emit no work here.
            DispatchOutcome::DeferredToLegacy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_status_terminal_classification() {
        assert!(!CommandStatus::Pending.is_terminal());
        assert!(!CommandStatus::Active.is_terminal());
        assert!(CommandStatus::Completed.is_terminal());
        assert!(CommandStatus::Failed(CommandFailure::Unreachable).is_terminal());
        assert!(CommandStatus::Superseded.is_terminal());
    }

    #[test]
    fn id_generator_is_monotonic() {
        let mut gen = PlayerCommandIdGen::default();
        let a = gen.allocate();
        let b = gen.allocate();
        let c = gen.allocate();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
    }
}
