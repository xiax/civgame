//! Heal-pipeline data layer (Heal-1).
//!
//! Tracks combat-induced damage as an `Injury` component derived from
//! the gap between `Health.current` and `Health.max`. The component
//! is what `HealNeedScorer` (Heal-2) reads to gate `AgentGoal::SeekCare`,
//! and what `ProvideCareScorer` reads to find treatable patients.
//!
//! Injury severity tracks the *peak* damage taken — `max - current`
//! at the most recent damage write. When the heal pipeline (Heal-3)
//! restores `Health.current`, the same system clears the component.
//! No combat-system edit required: insertion is reactive on
//! `Changed<Health>`.

use bevy::prelude::*;

use crate::simulation::combat::Health;
use crate::simulation::faction::FactionMember;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI, Profession};
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::TILE_SIZE;

/// Per-agent injury record. Stamped by `injury_tracking_system` when
/// `Health.current < Health.max`; removed when fully healed.
///
/// `severity` is the `max - current` gap at the most recent damage
/// tick. `applied_tick` captures the first damage event of the
/// current injury (across consecutive damage windows); reset only
/// when the component is fully cleared. `last_damage_tick` is the
/// most recent damage write — triage scorers prefer fresher wounds
/// (an old scar isn't urgent).
#[derive(Component, Clone, Copy, Debug)]
pub struct Injury {
    pub severity: u8,
    pub applied_tick: u64,
    pub last_damage_tick: u64,
}

/// Sequential schedule, after `combat::combat_system` so the same
/// frame's damage is reflected. Reactive on `Changed<Health>` —
/// every damage write reactivates the filter, fires this system
/// once per affected entity.
pub fn injury_tracking_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut query: Query<(Entity, &Health, Option<&mut Injury>), Changed<Health>>,
) {
    let now = clock.tick;
    for (entity, health, injury_opt) in query.iter_mut() {
        let missing = health.max.saturating_sub(health.current);
        match (missing, injury_opt) {
            (0, Some(_)) => {
                commands.entity(entity).remove::<Injury>();
            }
            (0, None) => {}
            (missing, Some(mut injury)) => {
                if missing != injury.severity {
                    injury.severity = missing;
                    injury.last_damage_tick = now;
                }
            }
            (missing, None) => {
                commands.entity(entity).insert(Injury {
                    severity: missing,
                    applied_tick: now,
                    last_damage_tick: now,
                });
            }
        }
    }
}

/// Severity points decremented per tick while the Healer is adjacent
/// to the patient. At 1/tick @ 20 Hz, a severity-100 wound clears in
/// ~5 seconds. Future Heal-3b: scale by `Skills::Medicine` competence
/// and gate on a medicine resource consumed from inventory.
pub const HEAL_SEVERITY_PER_TICK: u8 = 1;
/// Medicine XP granted to the Healer per tick of adjacent treatment.
pub const HEAL_MEDICINE_XP_PER_TICK: u32 = 1;
/// Chebyshev radius within which the Healer can treat the patient.
pub const HEAL_ADJACENCY_RADIUS: i32 = 1;
/// Maximum chebyshev radius the `htn_provide_care_dispatch_system`
/// will scan for patients from the Healer's tile. Mirrors the
/// `PARTNER_RADIUS = 12` used by socialize/play dispatchers.
pub const HEAL_SCAN_RADIUS: i32 = 12;

/// HTN dispatcher for `AgentGoal::ProvideCare`. Iterates Healers
/// (and Apprentices targeting Medicine) under the ProvideCare goal,
/// finds the nearest same-faction patient with an `Injury`, and
/// dispatches `Task::Heal { patient }` via the standard routing
/// helper. ParallelB schedule, mirroring the other goal-driven
/// dispatchers.
#[allow(clippy::too_many_arguments)]
pub fn htn_provide_care_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    injured_query: Query<(Entity, &Transform, &FactionMember), With<Injury>>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &Transform,
            &FactionMember,
            &Profession,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    for (agent, mut ai, mut aq, goal, transform, member, profession, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !matches!(*goal, AgentGoal::ProvideCare) {
            continue;
        }
        if !matches!(*profession, Profession::Healer | Profession::Apprentice) {
            continue;
        }
        if ai.state != AiState::Idle || ai.task_id != PersonAI::UNEMPLOYED {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        let mut best: Option<(Entity, (i32, i32), i32)> = None;
        for (patient, patient_t, patient_member) in injured_query.iter() {
            if patient == agent {
                continue;
            }
            if patient_member.faction_id != member.faction_id {
                continue;
            }
            let px = (patient_t.translation.x / TILE_SIZE).floor() as i32;
            let py = (patient_t.translation.y / TILE_SIZE).floor() as i32;
            let d = (px - cur_tx).abs().max((py - cur_ty).abs());
            if d > HEAL_SCAN_RADIUS {
                continue;
            }
            if best.map_or(true, |(_, _, bd)| d < bd) {
                best = Some((patient, (px, py), d));
            }
        }

        let Some((patient, patient_tile, _)) = best else {
            continue;
        };
        let dispatched = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            patient_tile,
            TaskKind::Heal,
            Some(patient),
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !dispatched {
            continue;
        }
        aq.dispatch(Task::Heal { patient });
    }
}

/// Sequential executor for `Task::Heal { patient }`. Runs after
/// `movement_system`. Verifies the Healer is still adjacent to the
/// targeted patient; if so, decrements `Injury.severity` and grants
/// Medicine XP. If the patient moved out of range or the injury
/// cleared, advances the action queue so the Healer's next goal
/// re-evaluation can pick a new patient.
#[allow(clippy::too_many_arguments)]
pub fn heal_task_system(
    mut healer_query: Query<(
        Entity,
        &mut ActionQueue,
        &mut PersonAI,
        &Transform,
        &mut Skills,
    )>,
    transform_query: Query<&Transform>,
    mut injury_query: Query<&mut Injury>,
    mut commands: Commands,
) {
    for (_healer, mut aq, mut ai, healer_t, mut skills) in healer_query.iter_mut() {
        let Task::Heal { patient } = aq.current else {
            continue;
        };
        if ai.state != AiState::Working {
            continue;
        }
        let Ok(patient_t) = transform_query.get(patient) else {
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };
        let hx = (healer_t.translation.x / TILE_SIZE).floor() as i32;
        let hy = (healer_t.translation.y / TILE_SIZE).floor() as i32;
        let px = (patient_t.translation.x / TILE_SIZE).floor() as i32;
        let py = (patient_t.translation.y / TILE_SIZE).floor() as i32;
        let d = (px - hx).abs().max((py - hy).abs());
        if d > HEAL_ADJACENCY_RADIUS {
            // Patient moved out of range; surrender this task so the
            // next dispatch cycle picks a fresh target.
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }
        let Ok(mut injury) = injury_query.get_mut(patient) else {
            // Patient was healed by another Healer or `Injury`
            // despawned for some other reason — release the task.
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };
        injury.severity = injury.severity.saturating_sub(HEAL_SEVERITY_PER_TICK);
        skills.gain_xp(SkillKind::Medicine, HEAL_MEDICINE_XP_PER_TICK);
        if injury.severity == 0 {
            commands.entity(patient).remove::<Injury>();
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_app() -> App {
        let mut app = App::new();
        app.insert_resource(SimClock::default());
        app.add_systems(Update, injury_tracking_system);
        app
    }

    #[test]
    fn damage_inserts_injury() {
        let mut app = build_app();
        let entity = app
            .world_mut()
            .spawn(Health {
                current: 100,
                max: 100,
            })
            .id();
        // First tick: no damage, no injury.
        app.update();
        assert!(app.world().get::<Injury>(entity).is_none());

        // Apply damage.
        let mut health = app.world_mut().get_mut::<Health>(entity).unwrap();
        health.current = 75;
        app.update();
        let injury = app
            .world()
            .get::<Injury>(entity)
            .expect("Injury inserted after damage");
        assert_eq!(injury.severity, 25);
    }

    #[test]
    fn additional_damage_updates_severity() {
        let mut app = build_app();
        let entity = app
            .world_mut()
            .spawn(Health {
                current: 75,
                max: 100,
            })
            .id();
        app.update();
        assert_eq!(app.world().get::<Injury>(entity).unwrap().severity, 25);

        let mut health = app.world_mut().get_mut::<Health>(entity).unwrap();
        health.current = 40;
        app.update();
        assert_eq!(app.world().get::<Injury>(entity).unwrap().severity, 60);
    }

    #[test]
    fn full_heal_removes_injury() {
        let mut app = build_app();
        let entity = app
            .world_mut()
            .spawn(Health {
                current: 50,
                max: 100,
            })
            .id();
        app.update();
        assert!(app.world().get::<Injury>(entity).is_some());

        let mut health = app.world_mut().get_mut::<Health>(entity).unwrap();
        health.current = 100;
        app.update();
        assert!(
            app.world().get::<Injury>(entity).is_none(),
            "Injury should clear when health.current == max"
        );
    }

    #[test]
    fn redundant_health_write_doesnt_flap_injury() {
        let mut app = build_app();
        let entity = app
            .world_mut()
            .spawn(Health {
                current: 80,
                max: 100,
            })
            .id();
        app.update();
        let applied_at = app.world().get::<Injury>(entity).unwrap().applied_tick;

        // Re-write the same value — Changed filter still fires, but
        // severity unchanged so `applied_tick` should not reset.
        let mut health = app.world_mut().get_mut::<Health>(entity).unwrap();
        health.current = 80;
        app.update();
        let injury = app.world().get::<Injury>(entity).unwrap();
        assert_eq!(injury.severity, 20);
        assert_eq!(injury.applied_tick, applied_at);
    }
}
