use crate::simulation::combat::CombatEvent;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::tasks::{task_interacts_from_adjacent, task_is_labor};
use crate::simulation::typed_task::ActionQueue;
use crate::world::terrain::tile_to_world;
use bevy::prelude::*;

#[derive(Component, Default)]
pub struct CombatAnimations {
    pub lunge_timer: f32,
    pub lunge_dir: Vec2,
    pub hit_timer: f32,
}

pub fn handle_combat_events(
    mut events: EventReader<CombatEvent>,
    mut commands: Commands,
    mut query: Query<(Entity, &Transform, Option<&mut CombatAnimations>)>,
) {
    for ev in events.read() {
        let target_pos = query
            .get(ev.target)
            .ok()
            .map(|(_, t, _)| t.translation.truncate());

        // Attacker lunge
        if let Some(target_pos) = target_pos {
            if let Ok((attacker_ent, attacker_transform, anim_opt)) = query.get_mut(ev.attacker) {
                let diff = target_pos - attacker_transform.translation.truncate();
                let dir = diff.normalize_or_zero();

                if let Some(mut anim) = anim_opt {
                    anim.lunge_timer = 0.2;
                    anim.lunge_dir = dir;
                } else {
                    commands.entity(attacker_ent).insert(CombatAnimations {
                        lunge_timer: 0.2,
                        lunge_dir: dir,
                        hit_timer: 0.0,
                    });
                }
            }
        }

        // Target hit
        if let Ok((target_ent, _, anim_opt)) = query.get_mut(ev.target) {
            if let Some(mut anim) = anim_opt {
                anim.hit_timer = 0.2;
            } else {
                commands.entity(target_ent).insert(CombatAnimations {
                    lunge_timer: 0.0,
                    lunge_dir: Vec2::ZERO,
                    hit_timer: 0.2,
                });
            }
        }
    }
}

/// Per-tile pixel lean applied to a worker's body sprite while it stands on a
/// `pick_adjacent_stand_tile` slot and works on an adjacent target. Stays well
/// inside one tile (`TILE_SIZE = 16`) so the sprite reads as "interacting"
/// without crossing onto the target tile or affecting collision/pathfinding.
pub const NUDGE_PX: f32 = 5.0;

/// Pure helper: compute the visual nudge offset for a worker whose body sprite
/// should lean toward its adjacent work target. Returns `Vec2::ZERO` whenever
/// the agent is not actively working a labor+adjacent task, or when the agent
/// is already standing on the destination tile.
pub fn interaction_nudge_offset(
    state: AiState,
    task_kind: u16,
    parent_xy: Vec2,
    dest_tile: (i32, i32),
) -> Vec2 {
    if state != AiState::Working {
        return Vec2::ZERO;
    }
    if !(task_interacts_from_adjacent(task_kind) && task_is_labor(task_kind)) {
        return Vec2::ZERO;
    }
    let dest_xy = tile_to_world(dest_tile.0, dest_tile.1);
    let dir = (dest_xy - parent_xy).normalize_or_zero();
    dir * NUDGE_PX
}

/// Drives the per-frame transform offsets for combat animations (lunge + hit shake).
/// Color (hit-flash red, fog tint, clothing alpha) is owned by
/// `apply_entity_fog_tint_system` — do not write `sprite.color` here.
///
/// The (0, -8) rest position assumes a single per-entity child sprite (Person /
/// animal). Vehicle children compose a multi-cell body at per-cell offsets;
/// `Without<VehicleCellTint>` excludes them so their layout isn't collapsed
/// every frame.
pub fn update_animations(
    time: Res<Time>,
    mut anim_query: Query<&mut CombatAnimations>,
    worker_q: Query<
        (&Transform, &PersonAI, &ActionQueue),
        (With<Person>, Without<super::entity_sprites::VisualChild>),
    >,
    mut visual_query: Query<
        (&Parent, &mut Transform),
        (
            With<super::entity_sprites::VisualChild>,
            Without<super::entity_sprites::VehicleCellTint>,
        ),
    >,
) {
    let dt = time.delta_secs();
    const BASE_Y: f32 = -8.0;

    for (parent, mut transform) in visual_query.iter_mut() {
        let parent_ent = parent.get();
        let nudge = match worker_q.get(parent_ent) {
            Ok((parent_t, ai, aq)) => interaction_nudge_offset(
                ai.state(),
                aq.current_task_kind(),
                parent_t.translation.truncate(),
                ai.dest_tile,
            ),
            Err(_) => Vec2::ZERO,
        };

        if let Ok(mut anim) = anim_query.get_mut(parent_ent) {
            let mut offset = Vec2::new(0.0, BASE_Y) + nudge;

            if anim.lunge_timer > 0.0 {
                anim.lunge_timer = (anim.lunge_timer - dt).max(0.0);
                let t = (0.2 - anim.lunge_timer) / 0.2;
                let lunge_dist = 12.0 * (1.0 - (t * 2.0 - 1.0).powi(2));
                offset += anim.lunge_dir * lunge_dist;
            }

            if anim.hit_timer > 0.0 {
                anim.hit_timer = (anim.hit_timer - dt).max(0.0);
                let shake_amount = 3.0;
                offset += Vec2::new(
                    (fastrand::f32() - 0.5) * shake_amount,
                    (fastrand::f32() - 0.5) * shake_amount,
                );
            }

            if transform.translation.x != offset.x || transform.translation.y != offset.y {
                transform.translation.x = offset.x;
                transform.translation.y = offset.y;
            }
        } else {
            let target_x = nudge.x;
            let target_y = BASE_Y + nudge.y;
            if transform.translation.x != target_x || transform.translation.y != target_y {
                transform.translation.x = target_x;
                transform.translation.y = target_y;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::tasks::TaskKind;
    use crate::world::terrain::TILE_SIZE;

    fn tile_center(x: i32, y: i32) -> Vec2 {
        Vec2::new(
            x as f32 * TILE_SIZE + TILE_SIZE * 0.5,
            y as f32 * TILE_SIZE + TILE_SIZE * 0.5,
        )
    }

    #[test]
    fn working_labor_adjacent_nudges_toward_dest() {
        let parent = tile_center(0, 0);
        let nudge = interaction_nudge_offset(
            AiState::Working,
            TaskKind::Gather as u16,
            parent,
            (2, 0),
        );
        assert!((nudge.x - NUDGE_PX).abs() < 1e-4, "x: {}", nudge.x);
        assert!(nudge.y.abs() < 1e-4, "y: {}", nudge.y);
    }

    #[test]
    fn idle_state_returns_zero() {
        let parent = tile_center(0, 0);
        let nudge =
            interaction_nudge_offset(AiState::Idle, TaskKind::Gather as u16, parent, (2, 0));
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn seeking_state_returns_zero() {
        let parent = tile_center(0, 0);
        let nudge =
            interaction_nudge_offset(AiState::Seeking, TaskKind::Gather as u16, parent, (2, 0));
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn routing_state_returns_zero() {
        let parent = tile_center(0, 0);
        let nudge = interaction_nudge_offset(
            AiState::Routing,
            TaskKind::HaulMaterials as u16,
            parent,
            (2, 0),
        );
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn socialize_is_adjacent_but_not_labor() {
        let parent = tile_center(0, 0);
        let nudge = interaction_nudge_offset(
            AiState::Working,
            TaskKind::Socialize as u16,
            parent,
            (2, 0),
        );
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn play_is_adjacent_but_not_labor() {
        let parent = tile_center(0, 0);
        let nudge =
            interaction_nudge_offset(AiState::Working, TaskKind::Play as u16, parent, (2, 0));
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn idle_task_returns_zero() {
        let parent = tile_center(0, 0);
        let nudge =
            interaction_nudge_offset(AiState::Working, TaskKind::Idle as u16, parent, (2, 0));
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn same_tile_returns_zero() {
        let parent = tile_center(2, 3);
        let nudge = interaction_nudge_offset(
            AiState::Working,
            TaskKind::Gather as u16,
            parent,
            (2, 3),
        );
        assert_eq!(nudge, Vec2::ZERO);
    }

    #[test]
    fn diagonal_direction_splits_evenly() {
        let parent = tile_center(0, 0);
        let nudge = interaction_nudge_offset(
            AiState::Working,
            TaskKind::Construct as u16,
            parent,
            (1, 1),
        );
        let expected = NUDGE_PX / 2f32.sqrt();
        assert!((nudge.x - expected).abs() < 1e-4, "x: {}", nudge.x);
        assert!((nudge.y - expected).abs() < 1e-4, "y: {}", nudge.y);
    }

    #[test]
    fn negative_direction_works() {
        let parent = tile_center(5, 5);
        let nudge = interaction_nudge_offset(
            AiState::Working,
            TaskKind::PrepareField as u16,
            parent,
            (4, 5),
        );
        assert!((nudge.x + NUDGE_PX).abs() < 1e-4, "x: {}", nudge.x);
        assert!(nudge.y.abs() < 1e-4, "y: {}", nudge.y);
    }
}
