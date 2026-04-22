use bevy::prelude::*;
use crate::economy::goods::Good;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::items::GroundItem;
use crate::simulation::jobs::JobKind;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

#[derive(Component, Clone, Copy, Debug)]
pub struct Health {
    pub current: u8,
    pub max:     u8,
}

impl Health {
    pub fn new(max: u8) -> Self { Self { current: max, max } }
    pub fn is_dead(self) -> bool { self.current == 0 }
    pub fn fraction(self) -> f32 { self.current as f32 / self.max as f32 }
}

#[derive(Component, Default, Clone, Copy)]
pub struct CombatTarget(pub Option<Entity>);

const ATTACK_DAMAGE: u8 = 2;

pub fn combat_system(
    spatial: Res<SpatialIndex>,
    attacker_query: Query<(Entity, &CombatTarget, &Transform, &LodLevel, &BucketSlot)>,
    mut health_query: Query<&mut Health>,
    mut ai_query: Query<&mut PersonAI>,
    clock: Res<SimClock>,
) {
    let mut damage_events: Vec<(Entity, u8)> = Vec::new();

    for (attacker, combat_target, transform, lod, slot) in &attacker_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let Some(target) = combat_target.0 else { continue };
        if target == attacker { continue; }

        if health_query.get(target).is_err() {
            if let Ok(mut ai) = ai_query.get_mut(attacker) {
                ai.state = AiState::Idle;
                ai.job_id = PersonAI::UNEMPLOYED;
            }
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut found = false;
        'find: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                for &e in spatial.get(tx + dx, ty + dy) {
                    if e == target {
                        found = true;
                        break 'find;
                    }
                }
            }
        }

        if found {
            damage_events.push((target, ATTACK_DAMAGE));
            if let Ok(mut ai) = ai_query.get_mut(attacker) {
                ai.state = AiState::Attacking;
            }
        } else {
            if let Ok(mut ai) = ai_query.get_mut(attacker) {
                if ai.state == AiState::Attacking {
                    ai.state = AiState::Idle;
                    ai.job_id = PersonAI::UNEMPLOYED;
                }
            }
        }
    }

    for (target, dmg) in damage_events {
        if let Ok(mut health) = health_query.get_mut(target) {
            health.current = health.current.saturating_sub(dmg);
        }
    }
}

pub fn death_system(
    mut commands: Commands,
    mut registry: ResMut<FactionRegistry>,
    mut clock: ResMut<SimClock>,
    query: Query<(Entity, &Health, &Transform, Option<&FactionMember>, Option<&Person>, Option<&Wolf>, Option<&Deer>)>,
) {
    for (entity, health, transform, member, person, wolf, deer) in &query {
        if !health.is_dead() { continue; }

        if let Some(fm) = member {
            registry.remove_member(fm.faction_id);
        }
        if person.is_some() {
            clock.population = clock.population.saturating_sub(1);
        }

        let loot_qty: Option<u8> = if wolf.is_some() { Some(1) }
            else if deer.is_some() { Some(3) }
            else { None };

        if let Some(qty) = loot_qty {
            commands.spawn((
                GroundItem { good: Good::Food, qty },
                *transform,
                GlobalTransform::default(),
            ));
        }

        commands.entity(entity).despawn();
    }
}

const HUNT_RADIUS: i32 = 15;
const HUNT_HUNGER_THRESHOLD: u8 = 120;

pub fn hunting_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    prey_transforms: Query<&Transform, Or<(With<Wolf>, With<Deer>)>>,
    prey_check: Query<(), Or<(With<Wolf>, With<Deer>)>>,
    mut hunters: Query<(
        &mut PersonAI,
        &mut CombatTarget,
        &Needs,
        &BucketSlot,
        &LodLevel,
        &Transform,
    ), With<Person>>,
) {
    for (mut ai, mut combat_target, needs, slot, lod, transform) in hunters.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        if let Some(prey) = combat_target.0 {
            if let Ok(prey_t) = prey_transforms.get(prey) {
                let ptx = (prey_t.translation.x / TILE_SIZE).floor() as i16;
                let pty = (prey_t.translation.y / TILE_SIZE).floor() as i16;
                ai.target_tile = (ptx, pty);
                if ai.state == AiState::Idle {
                    ai.state = AiState::Seeking;
                }
            } else {
                combat_target.0 = None;
            }
            continue;
        }

        if needs.hunger <= HUNT_HUNGER_THRESHOLD || ai.state != AiState::Idle {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        'find: for dy in -HUNT_RADIUS..=HUNT_RADIUS {
            for dx in -HUNT_RADIUS..=HUNT_RADIUS {
                for &candidate in spatial.get(tx + dx, ty + dy) {
                    if prey_check.get(candidate).is_ok() {
                        combat_target.0 = Some(candidate);
                        ai.target_tile = ((tx + dx) as i16, (ty + dy) as i16);
                        ai.state = AiState::Seeking;
                        ai.job_id = JobKind::Forager as u16;
                        break 'find;
                    }
                }
            }
        }
    }
}

