//! Phase-2 knowledge system: directed teaching, lectures, and reading.
//!
//! This module hosts the executors for:
//! - `TaskKind::Read` — solo study of a tablet/book in inventory.
//! - `TaskKind::Teach` / `AttendLecture` — 1-on-1 player-directed teaching.
//! - `TaskKind::HoldLecture` / `AttendLecture` — broadcast teaching to nearby
//!   same-faction adults drafted by `apply_lecture_request_system`.
//!
//! Progress is accumulated in `PersonKnowledge::study_progress`. When the
//! threshold (`study_threshold(tech) = complexity * STUDY_TICKS_PER_COMPLEXITY`)
//! is met, `PersonKnowledge::try_learn` runs and the tech moves to Learned —
//! there is no cap. Awareness is granted on the first study tick.
//!
//! Per-tick study rates (multiplied by `int_scale = 1.0 + INT_mod * 0.1`,
//! then divided by the student's `learning_slowdown` so heavy stacks absorb
//! more slowly):
//! - Solo read: 1
//! - Lecture attendance: 2
//! - 1-on-1 teach: 3
//!
//! Cancellation paths: lecturers losing `Lecturing` (death, distance) cause
//! the per-tick lecture system to release all their students. Teach pairs
//! self-clean when either side disappears or `ends_tick` is reached.

use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;

use crate::simulation::faction::{FactionMember, PlayerFaction};
use crate::simulation::knowledge::{study_threshold, PersonKnowledge, StudyOutcome};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI};
use crate::simulation::schedule::SimClock;
use crate::simulation::stats::{modifier, Stats};
use crate::simulation::tasks::TaskKind;
use crate::simulation::technology::{tech_def, TechId};
use crate::ui::activity_log::{ActivityEntryKind, ActivityLogEvent};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

/// Lecture duration in ticks (~30 s real-time at 20 Hz; ~5 in-game minutes).
pub const LECTURE_DURATION: u64 = 600;
/// Maximum students drafted per lecture.
pub const LECTURE_STUDENT_CAP: usize = 8;
/// Spatial scan radius (tiles) for lecture drafting.
pub const LECTURE_DRAFT_RADIUS: i32 = 6;
/// Distance (tiles) at which a student is considered out-of-range and released.
pub const LECTURE_LEAVE_DISTANCE: f32 = 8.0;
/// Per-tick "live" lecture system cadence — runs every tick (cheap; iterates
/// only entities with the `Attending` / `Lecturing` markers).

/// Duration of a player-directed 1-on-1 teaching session.
pub const TEACH_DURATION: u64 = 120;
/// Maximum tile distance between teacher and student during a `TeachingPair`.
pub const TEACH_MAX_DISTANCE: f32 = 2.5;

// ── Components ──────────────────────────────────────────────────────────────

/// Inserted on a lecturer for the duration of their lecture. Carries the
/// chosen tech and end tick. Also implies `Drafted` so autonomous goal
/// dispatch leaves them alone.
#[derive(Component, Clone, Copy, Debug)]
pub struct Lecturing {
    pub ends_tick: u64,
    pub tech: TechId,
    pub anchor: (i32, i32),
}

/// Inserted on a drafted student for the duration of a lecture they are
/// attending. Released by `lecture_tick_system` when the lecturer ends or
/// the student walks out of range.
#[derive(Component, Clone, Copy, Debug)]
pub struct Attending {
    pub lecturer: Entity,
    pub ends_tick: u64,
    pub tech: TechId,
}

/// Inserted on a teacher for a 1-on-1 lesson. The matching `BeingTaught`
/// lives on the student.
#[derive(Component, Clone, Copy, Debug)]
pub struct TeachingPair {
    pub student: Entity,
    pub tech: TechId,
    pub ends_tick: u64,
}

/// Inserted on a student during a 1-on-1 lesson.
#[derive(Component, Clone, Copy, Debug)]
pub struct BeingTaught {
    pub teacher: Entity,
    pub tech: TechId,
    pub ends_tick: u64,
}

// ── Resources ───────────────────────────────────────────────────────────────

/// Inspector "Hold Lecture" pulse. Set by the player UI; consumed by
/// `apply_lecture_request_system` next Economy tick.
#[derive(Resource, Default)]
pub struct LectureRequest(pub Option<(Entity, TechId)>);

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Read-progress study amount per tick, scaled by intelligence.
fn study_amount(stats: &Stats, base: f32) -> u32 {
    let int_scale = 1.0 + (modifier(stats.intelligence) as f32 * 0.1).max(-0.4);
    (base * int_scale).round().max(1.0) as u32
}

/// Find the inventory slot index of a tablet/book whose `tech_payload` matches
/// `tech`. Returns the first match.
fn find_readable_slot(agent: &EconomicAgent, tech: TechId) -> Option<usize> {
    let tablet = crate::economy::core_ids::ClayTablet.get().copied();
    let book = crate::economy::core_ids::Book.get().copied();
    for (i, (item, qty)) in agent.inventory.iter().enumerate() {
        if *qty == 0 {
            continue;
        }
        let rid = Some(item.resource_id);
        if rid != tablet && rid != book {
            continue;
        }
        if item.tech_payload == Some(tech) {
            return Some(i);
        }
    }
    None
}

/// True if a same-faction adult is plausibly draftable for a lecture: not
/// dormant, not currently lecturing, and not already mastering the tech.
fn lecture_candidate_ok(
    knowledge: &PersonKnowledge,
    lod: LodLevel,
    tech: TechId,
    is_self: bool,
    already_busy: bool,
) -> bool {
    if is_self || already_busy {
        return false;
    }
    if lod == LodLevel::Dormant {
        return false;
    }
    if knowledge.has_learned(tech) {
        return false;
    }
    true
}

// ── Systems ─────────────────────────────────────────────────────────────────

/// Solo reading: agent stands still, accumulating study progress against the
/// tech encoded on a tablet/book in their inventory. Awareness is granted
/// immediately; learning is gated on `study_threshold`. The item is never
/// consumed (both tablets and books are reusable per design).
pub fn read_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    player: Res<PlayerFaction>,
    mut activity_log: EventWriter<ActivityLogEvent>,
    mut q: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut PersonKnowledge,
        &Stats,
        &EconomicAgent,
        Option<&FactionMember>,
        &LodLevel,
    )>,
) {
    let now = clock.tick as u32;
    for (entity, mut ai, mut aq, mut knowledge, stats, agent, fm, lod) in q.iter_mut() {
        if aq.current_task_kind() != TaskKind::Read as u16 || ai.state != AiState::Working {
            continue;
        }
        if *lod == LodLevel::Dormant {
            continue;
        }
        let Some(tech) = aq.current.knowledge_tech() else {
            // Misconfigured task — clear it.
            aq.finish_task(&mut ai);
            continue;
        };
        if find_readable_slot(agent, tech).is_none() {
            // Lost the tablet (dropped, traded). End task.
            aq.finish_task(&mut ai);
            continue;
        }

        let amount = study_amount(stats, 1.0);
        let outcome = knowledge.add_study_progress(tech, amount, stats, now);

        ai.work_progress = ai.work_progress.saturating_add(1);

        let session_done = ai.work_progress >= 60;
        let learned = matches!(outcome, StudyOutcome::Learned);
        if learned {
            if let Some(fm) = fm {
                if fm.faction_id == player.faction_id {
                    activity_log.send(ActivityLogEvent {
                        tick: clock.tick,
                        actor: entity,
                        faction_id: fm.faction_id,
                        kind: ActivityEntryKind::Read {
                            tech_name: tech_def(tech).name,
                        },
                    });
                }
            }
        }
        if session_done || learned {
            aq.finish_task(&mut ai);
            // Release the player-imposed `Drafted` marker so autonomous goal
            // dispatch can pick the agent back up.
            if let Some(mut ec) = commands.get_entity(entity) {
                ec.remove::<Drafted>();
            }
        }
    }
}

/// 1-on-1 teaching: while teacher and student remain adjacent, accumulate
/// progress on the student's `study_progress`. Ends when the timer expires,
/// the student learns, or one party disappears.
pub fn teach_task_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    player: Res<PlayerFaction>,
    mut activity_log: EventWriter<ActivityLogEvent>,
    transforms: Query<&Transform>,
    name_query: Query<&Name>,
    members: Query<&FactionMember>,
    mut teachers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            Option<&TeachingPair>,
        ),
        With<PersonAI>,
    >,
    mut students_kn: Query<(&mut PersonKnowledge, &Stats), Without<TeachingPair>>,
) {
    let now = clock.tick as u32;

    // Snapshot teacher → (student, tech, ends_tick) pairs.
    let pairs: Vec<(Entity, Entity, TechId, u64)> = teachers
        .iter()
        .filter_map(|(e, _ai, _aq, tp)| tp.map(|tp| (e, tp.student, tp.tech, tp.ends_tick)))
        .collect();

    if pairs.is_empty() {
        return;
    }

    for (teacher_e, student_e, tech, ends_tick) in pairs {
        // Guard adjacency.
        let Ok(t_tf) = transforms.get(teacher_e) else {
            cleanup_teach(&mut commands, teacher_e, student_e);
            continue;
        };
        let Ok(s_tf) = transforms.get(student_e) else {
            cleanup_teach(&mut commands, teacher_e, student_e);
            continue;
        };
        let dx = (t_tf.translation.x - s_tf.translation.x) / TILE_SIZE;
        let dy = (t_tf.translation.y - s_tf.translation.y) / TILE_SIZE;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist > TEACH_MAX_DISTANCE {
            // Not adjacent yet — let movement handle it; don't credit
            // progress this tick.
            if clock.tick >= ends_tick {
                cleanup_teach(&mut commands, teacher_e, student_e);
            }
            continue;
        }

        let mut learned = false;
        if let Ok((mut k, stats)) = students_kn.get_mut(student_e) {
            let amount = study_amount(stats, 3.0);
            let outcome = k.add_study_progress(tech, amount, stats, now);
            learned = matches!(outcome, StudyOutcome::Learned);
        }

        if learned {
            if let Ok(fm) = members.get(student_e) {
                if fm.faction_id == player.faction_id {
                    let student_name = name_query
                        .get(student_e)
                        .map(|n| n.as_str().to_string())
                        .unwrap_or_else(|_| "Someone".to_string());
                    activity_log.send(ActivityLogEvent {
                        tick: clock.tick,
                        actor: teacher_e,
                        faction_id: fm.faction_id,
                        kind: ActivityEntryKind::Taught {
                            student_name,
                            tech_name: tech_def(tech).name,
                        },
                    });
                }
            }
        }

        if learned || clock.tick >= ends_tick {
            cleanup_teach(&mut commands, teacher_e, student_e);
        } else {
            // Pin the teacher to "Working" so movement doesn't drift them.
            if let Ok((_, mut ai, mut aq, _)) = teachers.get_mut(teacher_e) {
                let prev_progress = ai.work_progress.saturating_add(1);
                aq.current = crate::simulation::typed_task::Task::Teach { tech };
                aq.begin_working(&mut ai);
                ai.work_progress = prev_progress;
            }
        }
    }
}

fn cleanup_teach(commands: &mut Commands, teacher_e: Entity, student_e: Entity) {
    if let Some(mut ec) = commands.get_entity(teacher_e) {
        ec.remove::<TeachingPair>();
        ec.remove::<Drafted>();
    }
    if let Some(mut ec) = commands.get_entity(student_e) {
        ec.remove::<BeingTaught>();
        ec.remove::<Drafted>();
    }
}

/// Drafts up to `LECTURE_STUDENT_CAP` nearby same-faction adults around the
/// lecturer. Drops their `ActivePlan` (mirror of `apply_muster_hunters_system`)
/// and inserts `Attending`+`Drafted`. Inserts `Lecturing`+`Drafted` on the
/// lecturer themself.
pub fn apply_lecture_request_system(
    mut commands: Commands,
    mut request: ResMut<LectureRequest>,
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    transforms: Query<&Transform>,
    members: Query<&FactionMember>,
    qq: Query<(
        &PersonKnowledge,
        &LodLevel,
        Option<&Lecturing>,
        Option<&Attending>,
        Option<&BeingTaught>,
    )>,
) {
    let Some((lecturer_e, tech)) = request.0.take() else {
        return;
    };
    let Ok(lec_tf) = transforms.get(lecturer_e) else {
        return;
    };
    let Ok(lec_member) = members.get(lecturer_e) else {
        return;
    };
    let lec_faction = lec_member.faction_id;

    let anchor_tx = (lec_tf.translation.x / TILE_SIZE).floor() as i32;
    let anchor_ty = (lec_tf.translation.y / TILE_SIZE).floor() as i32;
    let ends_tick = clock.tick + LECTURE_DURATION;

    if let Some(mut ec) = commands.get_entity(lecturer_e) {
        ec.insert(Lecturing {
            ends_tick,
            tech,
            anchor: (anchor_tx, anchor_ty),
        });
        ec.insert(Drafted);
    }

    // Spatial scan for candidate students.
    let mut drafted = 0usize;
    'outer: for dy in -LECTURE_DRAFT_RADIUS..=LECTURE_DRAFT_RADIUS {
        for dx in -LECTURE_DRAFT_RADIUS..=LECTURE_DRAFT_RADIUS {
            let tile_x = anchor_tx + dx;
            let tile_y = anchor_ty + dy;
            for &candidate in spatial.get(tile_x, tile_y) {
                if drafted >= LECTURE_STUDENT_CAP {
                    break 'outer;
                }
                if candidate == lecturer_e {
                    continue;
                }
                let Ok(fm) = members.get(candidate) else {
                    continue;
                };
                if fm.faction_id != lec_faction {
                    continue;
                }
                let Ok((knowledge, lod, lec, att, being)) = qq.get(candidate) else {
                    continue;
                };
                let busy = lec.is_some() || att.is_some() || being.is_some();
                if !lecture_candidate_ok(knowledge, *lod, tech, false, busy) {
                    continue;
                }
                if let Some(mut ec) = commands.get_entity(candidate) {
                    ec.insert(Attending {
                        lecturer: lecturer_e,
                        ends_tick,
                        tech,
                    });
                    ec.insert(Drafted);
                }
                drafted += 1;
            }
        }
    }
}

/// Per-tick lecture progress + cleanup. Awards study progress to every
/// attending student each tick; releases students whose lecturer disappeared,
/// is out of range, or whose `ends_tick` has elapsed.
pub fn lecture_tick_system(
    clock: Res<SimClock>,
    mut commands: Commands,
    player: Res<PlayerFaction>,
    mut activity_log: EventWriter<ActivityLogEvent>,
    transforms: Query<&Transform>,
    name_query: Query<&Name>,
    members: Query<&FactionMember>,
    mut students: Query<
        (
            Entity,
            &Attending,
            &mut PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &mut PersonKnowledge,
            &Stats,
        ),
        Without<Lecturing>,
    >,
    mut lecturers: Query<
        (
            Entity,
            &Lecturing,
            &mut PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
        ),
        Without<Attending>,
    >,
) {
    let now = clock.tick as u32;

    // Process students.
    for (student_e, att, mut ai, mut aq, mut knowledge, stats) in students.iter_mut() {
        let lecturer_present = transforms.get(att.lecturer).is_ok();
        let in_range = match (transforms.get(student_e), transforms.get(att.lecturer)) {
            (Ok(s), Ok(l)) => {
                let dx = (s.translation.x - l.translation.x) / TILE_SIZE;
                let dy = (s.translation.y - l.translation.y) / TILE_SIZE;
                (dx * dx + dy * dy).sqrt() <= LECTURE_LEAVE_DISTANCE
            }
            _ => false,
        };

        if !lecturer_present || !in_range || clock.tick >= att.ends_tick {
            // Release.
            if let Some(mut ec) = commands.get_entity(student_e) {
                ec.remove::<Attending>();
                ec.remove::<Drafted>();
            }
            aq.finish_task(&mut ai);
            continue;
        }

        // Pin task each tick.
        aq.begin_working(&mut ai);
        aq.current = crate::simulation::typed_task::Task::AttendLecture { tech: att.tech };

        let amount = study_amount(stats, 2.0);
        let outcome = knowledge.add_study_progress(att.tech, amount, stats, now);
        if matches!(outcome, StudyOutcome::Learned) {
            if let Ok(fm) = members.get(student_e) {
                if fm.faction_id == player.faction_id {
                    let student_name = name_query
                        .get(student_e)
                        .map(|n| n.as_str().to_string())
                        .unwrap_or_else(|_| "Someone".to_string());
                    activity_log.send(ActivityLogEvent {
                        tick: clock.tick,
                        actor: att.lecturer,
                        faction_id: fm.faction_id,
                        kind: ActivityEntryKind::Taught {
                            student_name,
                            tech_name: tech_def(att.tech).name,
                        },
                    });
                }
            }
            // Release student early on success.
            if let Some(mut ec) = commands.get_entity(student_e) {
                ec.remove::<Attending>();
                ec.remove::<Drafted>();
            }
            aq.finish_task(&mut ai);
        }
    }

    // Process lecturers (timeouts).
    for (lec_e, lec, mut ai, mut aq) in lecturers.iter_mut() {
        if clock.tick >= lec.ends_tick {
            if let Some(mut ec) = commands.get_entity(lec_e) {
                ec.remove::<Lecturing>();
                ec.remove::<Drafted>();
            }
            aq.finish_task(&mut ai);
        } else {
            // Pin lecturer in HoldLecture state.
            aq.begin_working(&mut ai);
            aq.current = crate::simulation::typed_task::Task::HoldLecture { tech: lec.tech };
        }
    }

    // Help silence unused-import warning when build doesn't reach this branch.
    let _ = study_threshold;
}

/// Inserts `TeachingPair` + `BeingTaught` once a teacher (assigned
/// `TaskKind::Teach` via `PlayerOrderKind::Teach`) reaches their student.
/// Idempotent — if the teacher already has a pair, this is a no-op.
pub fn apply_teach_order_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    transforms: Query<&Transform>,
    knowledges: Query<&PersonKnowledge>,
    teachers: Query<(
        Entity,
        &PersonAI,
        &crate::simulation::typed_task::ActionQueue,
        Option<&TeachingPair>,
    )>,
) {
    for (teacher_e, ai, aq, pair) in teachers.iter() {
        if pair.is_some() {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Teach as u16 {
            continue;
        }
        let Some(student_e) = ai.target_entity else {
            continue;
        };
        // Adjacency check.
        let Ok(t_tf) = transforms.get(teacher_e) else {
            continue;
        };
        let Ok(s_tf) = transforms.get(student_e) else {
            continue;
        };
        let dx = (t_tf.translation.x - s_tf.translation.x) / TILE_SIZE;
        let dy = (t_tf.translation.y - s_tf.translation.y) / TILE_SIZE;
        if (dx * dx + dy * dy).sqrt() > TEACH_MAX_DISTANCE {
            continue;
        }

        // Tech selection: highest-complexity in teacher.learned & !student.learned.
        let Ok(t_kn) = knowledges.get(teacher_e) else {
            continue;
        };
        let Ok(s_kn) = knowledges.get(student_e) else {
            continue;
        };
        let teachable = t_kn.learned.difference(&s_kn.learned);
        if teachable.is_empty() {
            continue;
        }
        let mut chosen: Option<(TechId, u8)> = None;
        for id in teachable.iter() {
            let cx = crate::simulation::technology::complexity(id);
            match chosen {
                None => chosen = Some((id, cx)),
                Some((_, best_cx)) if cx > best_cx => chosen = Some((id, cx)),
                _ => {}
            }
        }
        let Some((tech, _)) = chosen else { continue };

        let ends_tick = clock.tick + TEACH_DURATION;
        if let Some(mut ec) = commands.get_entity(teacher_e) {
            ec.insert(TeachingPair {
                student: student_e,
                tech,
                ends_tick,
            });
            ec.insert(Drafted);
        }
        if let Some(mut ec) = commands.get_entity(student_e) {
            ec.insert(BeingTaught {
                teacher: teacher_e,
                tech,
                ends_tick,
            });
            ec.insert(Drafted);
        }
    }
}

// ─── Pluralist Economy R8 follow-on: SelfActualization teaching trigger ───

/// Per-tick `Needs.self_actualization` increment when an agent
/// triggers a lecture. Mirrors `ESTEEM_POSTING_GAIN` shape: the
/// act of teaching grants the Maslow Tier-5 satisfaction.
pub const SELF_ACTUALIZATION_LECTURE_GAIN: f32 = 30.0;

/// Cadence at which `self_actualization_teaching_system` runs.
/// Once per game-day; at most one lecture is scheduled per firing
/// because `LectureRequest` is a single-slot resource.
pub const SELF_ACTUALIZATION_CADENCE: u64 = crate::world::seasons::TICKS_PER_DAY as u64;

/// Pluralist Economy R8 follow-on: SelfActualization tier triggers
/// teaching. When an agent's Maslow tier is `SelfActualization`
/// (every lower tier — including Esteem — satisfied) AND they have
/// at least one Learned tech, set `LectureRequest` to schedule a
/// lecture. The existing `apply_lecture_request_system` (Economy)
/// then drafts up to `LECTURE_STUDENT_CAP` nearby same-faction
/// adults and starts the lecture loop.
///
/// **Critical**: like `esteem_driven_posting_system`, this system
/// is *additive*. It does **not** preempt `goal_update_system`'s
/// goal selection — the lecturer's `AgentGoal` is unchanged at the
/// trigger point. The lecture mechanic itself inserts `Drafted`,
/// which suspends the agent's normal autonomy via the existing
/// `Without<Drafted>` filters in `goal_update_system`.
///
/// `LectureRequest` is a single-slot resource. Only one agent can
/// trigger per tick — first-eligible-agent wins. The system runs
/// at game-day cadence so contention is minimal.
pub fn self_actualization_teaching_system(
    clock: Res<SimClock>,
    mut request: ResMut<LectureRequest>,
    mut q: Query<(
        Entity,
        &mut crate::simulation::needs::Needs,
        &PersonKnowledge,
        &LodLevel,
        &FactionMember,
    )>,
) {
    use crate::simulation::goals::MaslowTier;

    if clock.tick % SELF_ACTUALIZATION_CADENCE != 0 {
        return;
    }
    // Already a lecture queued by the player UI? Don't override.
    if request.0.is_some() {
        return;
    }

    for (entity, mut needs, knowledge, lod, member) in q.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if member.faction_id == crate::simulation::faction::SOLO {
            continue;
        }
        if MaslowTier::next_unmet(&needs) != Some(MaslowTier::SelfActualization) {
            continue;
        }
        // Pick a Learned tech to teach. Prefer the highest-complexity
        // one (most "valuable" knowledge transferred first), mirroring
        // `tech_teaching_system`'s teach-rate weighting.
        let mut best: Option<(TechId, u8)> = None;
        for tech in knowledge.learned.iter() {
            let c = crate::simulation::technology::complexity(tech);
            if best.map_or(true, |(_, b)| c > b) {
                best = Some((tech, c));
            }
        }
        let Some((tech, _)) = best else {
            // Nothing Learned to teach — agent's Maslow ladder
            // can't satisfy via lecture today. Self_actualization
            // stays unmet; future ticks may find another agent.
            continue;
        };

        request.0 = Some((entity, tech));
        needs.self_actualization =
            (needs.self_actualization + SELF_ACTUALIZATION_LECTURE_GAIN).min(255.0);
        // Single-slot resource: stop after the first match.
        return;
    }
}
