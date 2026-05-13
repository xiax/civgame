//! Heal-pipeline data layer (Heal-1).
//!
//! Tracks combat-induced damage as an `Injury` component derived from
//! `Body.fraction()` — i.e. cumulative limb damage on humanoid agents.
//! Persons in this sim don't carry `Health`; they use a per-limb
//! `Body` component, so the watcher reacts to `Changed<Body>`. The
//! component is what `HealNeedScorer` (Heal-2) reads to gate
//! `AgentGoal::SeekCare`, and what `ProvideCareScorer` reads to find
//! treatable patients.
//!
//! Severity is `((1.0 - body.fraction()) * 255).round().clamp(0, 255)`
//! — a fully-intact body returns 0, a half-broken body returns 128,
//! a near-dead one returns near 255. When the heal pipeline (Heal-3)
//! restores limb HP, the same system clears the component.
//! No combat-system edit required: insertion is reactive on
//! `Changed<Body>`.

use bevy::prelude::*;

use crate::simulation::combat::Body;
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

/// Convert a `Body` into a 0-255 severity value. Returns 0 for a
/// fully-intact body, scales linearly with `1.0 - body.fraction()`,
/// clamps to 255. Pulled out so the system body stays focused on
/// the insert / update / remove decision tree.
#[inline]
pub fn severity_from_body(body: &Body) -> u8 {
    let frac = body.fraction().clamp(0.0, 1.0);
    let severity = ((1.0 - frac) * 255.0).round();
    severity.clamp(0.0, 255.0) as u8
}

/// Sequential schedule, after `combat::combat_system` so the same
/// frame's damage is reflected. Reactive on `Changed<Body>` — every
/// limb-damage write reactivates the filter, fires this system once
/// per affected entity.
pub fn injury_tracking_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    mut query: Query<(Entity, &Body, Option<&mut Injury>), Changed<Body>>,
) {
    let now = clock.tick;
    for (entity, body, injury_opt) in query.iter_mut() {
        let severity = severity_from_body(body);
        match (severity, injury_opt) {
            (0, Some(_)) => {
                commands.entity(entity).remove::<Injury>();
            }
            (0, None) => {}
            (severity, Some(mut injury)) => {
                if severity != injury.severity {
                    injury.severity = severity;
                    injury.last_damage_tick = now;
                }
            }
            (severity, None) => {
                commands.entity(entity).insert(Injury {
                    severity,
                    applied_tick: now,
                    last_damage_tick: now,
                });
            }
        }
    }
}

/// Limb HP restored per tick while the Healer is adjacent to the
/// patient. The Healer cycles through limbs in `Body.parts` order,
/// adding 1 HP to the first damaged limb each tick. A 30-HP leg
/// brought from 0 → 30 takes ~1.5 seconds @ 20 Hz; severity falls
/// proportionally as `injury_tracking_system` re-derives it from
/// `Body.fraction()`. Future Heal-3b: scale by `Skills::Medicine`
/// competence and gate on a medicine resource consumed from
/// inventory.
pub const HEAL_LIMB_HP_PER_TICK: u8 = 1;
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
/// `combat_system`. Verifies the Healer is still adjacent to the
/// targeted patient; if so, restores limb HP on the patient's
/// `Body` (which `injury_tracking_system` then translates back into
/// a lower `Injury.severity` on the next frame) and grants Medicine
/// XP. If the patient moved out of range or fully recovered,
/// advances the action queue so the Healer's next goal re-evaluation
/// can pick a new patient.
#[allow(clippy::too_many_arguments)]
pub fn heal_task_system(
    mut healer_query: Query<(
        Entity,
        &mut ActionQueue,
        &mut PersonAI,
        &Transform,
        &mut Skills,
        Option<&crate::simulation::apprenticeship::ApprenticeOf>,
    )>,
    transform_query: Query<&Transform>,
    mut body_query: Query<&mut Body>,
) {
    for (_healer, mut aq, mut ai, healer_t, mut skills, apprentice_opt) in healer_query.iter_mut() {
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
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }
        let Ok(mut body) = body_query.get_mut(patient) else {
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };
        let mut healed_something = false;
        for limb in body.parts.iter_mut() {
            if limb.current < limb.max {
                limb.current = limb.current.saturating_add(HEAL_LIMB_HP_PER_TICK).min(limb.max);
                healed_something = true;
                break;
            }
        }
        if healed_something {
            let xp = crate::simulation::apprenticeship::xp_with_apprentice_bonus(
                HEAL_MEDICINE_XP_PER_TICK,
                apprentice_opt,
            );
            skills.gain_xp(SkillKind::Medicine, xp);
            // injury_tracking_system observes the body change next
            // frame and despawns Injury when fraction reaches 1.0.
        } else {
            // Body fully intact — patient recovered.
            aq.advance();
            ai.task_id = PersonAI::UNEMPLOYED;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::combat::BodyPart;

    fn build_app() -> App {
        let mut app = App::new();
        app.insert_resource(SimClock::default());
        app.add_systems(Update, injury_tracking_system);
        app
    }

    fn damage_torso(app: &mut App, entity: Entity, dmg: u8) {
        let mut body = app.world_mut().get_mut::<Body>(entity).unwrap();
        let torso = body.get_mut(BodyPart::Torso);
        torso.current = torso.current.saturating_sub(dmg);
    }

    #[test]
    fn damage_inserts_injury() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        // First tick: undamaged body, no injury.
        app.update();
        assert!(app.world().get::<Injury>(entity).is_none());

        damage_torso(&mut app, entity, 20);
        app.update();
        let injury = app
            .world()
            .get::<Injury>(entity)
            .expect("Injury inserted after Body damage");
        assert!(injury.severity > 0);
    }

    #[test]
    fn additional_damage_updates_severity() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        damage_torso(&mut app, entity, 10);
        app.update();
        let first = app.world().get::<Injury>(entity).unwrap().severity;
        damage_torso(&mut app, entity, 15);
        app.update();
        let second = app.world().get::<Injury>(entity).unwrap().severity;
        assert!(second > first, "more damage must raise severity");
    }

    #[test]
    fn full_heal_removes_injury() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        damage_torso(&mut app, entity, 10);
        app.update();
        assert!(app.world().get::<Injury>(entity).is_some());

        // Restore the limb.
        let mut body = app.world_mut().get_mut::<Body>(entity).unwrap();
        let torso = body.get_mut(BodyPart::Torso);
        torso.current = torso.max;
        app.update();
        assert!(
            app.world().get::<Injury>(entity).is_none(),
            "Injury should clear when Body.fraction() == 1.0"
        );
    }

    #[test]
    fn redundant_body_write_doesnt_flap_injury() {
        let mut app = build_app();
        let entity = app.world_mut().spawn(Body::new_humanoid()).id();
        damage_torso(&mut app, entity, 10);
        app.update();
        let applied_at = app.world().get::<Injury>(entity).unwrap().applied_tick;
        let severity_before = app.world().get::<Injury>(entity).unwrap().severity;

        // Re-write Body without changing limb values — Changed
        // filter still fires, but severity unchanged so applied_tick
        // should not reset.
        let _ = app.world_mut().get_mut::<Body>(entity).unwrap();
        app.update();
        let injury = app.world().get::<Injury>(entity).unwrap();
        assert_eq!(injury.severity, severity_before);
        assert_eq!(injury.applied_tick, applied_at);
    }

    #[test]
    fn severity_from_body_endpoints() {
        let intact = Body::new_humanoid();
        assert_eq!(severity_from_body(&intact), 0);

        // Destroy every limb → fraction 0 → severity 255.
        let mut wrecked = Body::new_humanoid();
        for limb in wrecked.parts.iter_mut() {
            limb.current = 0;
        }
        assert_eq!(severity_from_body(&wrecked), 255);
    }
}
