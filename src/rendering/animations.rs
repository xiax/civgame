use crate::simulation::combat::CombatEvent;
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

/// Drives the per-frame transform offsets for combat animations (lunge + hit shake).
/// Color (hit-flash red, fog tint, clothing alpha) is owned by
/// `apply_entity_fog_tint_system` — do not write `sprite.color` here.
pub fn update_animations(
    time: Res<Time>,
    mut anim_query: Query<&mut CombatAnimations>,
    mut visual_query: Query<
        (&Parent, &mut Transform),
        With<super::entity_sprites::VisualChild>,
    >,
) {
    let dt = time.delta_secs();
    const BASE_Y: f32 = -8.0;

    for (parent, mut transform) in visual_query.iter_mut() {
        if let Ok(mut anim) = anim_query.get_mut(parent.get()) {
            let mut offset = Vec2::new(0.0, BASE_Y);

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
        } else if transform.translation.x != 0.0 || transform.translation.y != BASE_Y {
            transform.translation.x = 0.0;
            transform.translation.y = BASE_Y;
        }
    }
}
