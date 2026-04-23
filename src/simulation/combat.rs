use bevy::prelude::*;
use crate::economy::goods::Good;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::items::{GroundItem, Equipment, EquipmentSlot, WeaponStats, ArmorStats};
use crate::simulation::jobs::JobKind;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::RelationshipMemory;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyPart {
    Head = 0,
    Torso = 1,
    LeftArm = 2,
    RightArm = 3,
    LeftLeg = 4,
    RightLeg = 5,
}

impl BodyPart {
    pub const ALL: [BodyPart; 6] = [
        BodyPart::Head, BodyPart::Torso, BodyPart::LeftArm,
        BodyPart::RightArm, BodyPart::LeftLeg, BodyPart::RightLeg
    ];
    pub fn random() -> Self {
        let r = fastrand::u8(0..100);
        if r < 10 { BodyPart::Head }
        else if r < 50 { BodyPart::Torso }
        else if r < 62 { BodyPart::LeftArm }
        else if r < 74 { BodyPart::RightArm }
        else if r < 87 { BodyPart::LeftLeg }
        else { BodyPart::RightLeg }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LimbHealth {
    pub current: u8,
    pub max: u8,
}

impl LimbHealth {
    pub fn new(max: u8) -> Self { Self { current: max, max } }
    pub fn is_destroyed(&self) -> bool { self.current == 0 }
}

#[derive(Component, Clone, Debug)]
pub struct Body {
    pub parts: [LimbHealth; 6],
}

impl Body {
    pub fn new_humanoid() -> Self {
        let mut parts = [LimbHealth::new(20); 6];
        parts[BodyPart::Head as usize] = LimbHealth::new(20);
        parts[BodyPart::Torso as usize] = LimbHealth::new(40);
        parts[BodyPart::LeftArm as usize] = LimbHealth::new(20);
        parts[BodyPart::RightArm as usize] = LimbHealth::new(20);
        parts[BodyPart::LeftLeg as usize] = LimbHealth::new(30);
        parts[BodyPart::RightLeg as usize] = LimbHealth::new(30);
        Self { parts }
    }

    pub fn is_dead(&self) -> bool {
        self.parts[BodyPart::Head as usize].is_destroyed() ||
        self.parts[BodyPart::Torso as usize].is_destroyed()
    }

    pub fn get_mut(&mut self, part: BodyPart) -> &mut LimbHealth {
        &mut self.parts[part as usize]
    }

    pub fn fraction(&self) -> f32 {
        let total_current: u32 = self.parts.iter().map(|p| p.current as u32).sum();
        let total_max: u32 = self.parts.iter().map(|p| p.max as u32).sum();
        total_current as f32 / total_max as f32
    }
}

#[derive(Component, Default, Clone, Copy)]
pub struct CombatTarget(pub Option<Entity>);

const ATTACK_DAMAGE: u8 = 2;

pub fn combat_system(
    spatial: Res<SpatialIndex>,
    attacker_query: Query<(Entity, &CombatTarget, &Transform, &LodLevel, &BucketSlot, Option<&Equipment>)>,
    mut health_query: Query<&mut Health>,
    mut body_query: Query<&mut Body>,
    equipment_query: Query<&Equipment>,
    item_stats_query: Query<(Option<&WeaponStats>, Option<&ArmorStats>)>,
    mut ai_query: Query<&mut PersonAI>,
    mut rel_query: Query<Option<&mut RelationshipMemory>>,
    clock: Res<SimClock>,
) {
    // (target, attacker, damage)
    let mut health_damage_events: Vec<(Entity, Entity, u8)> = Vec::new();
    // (target, attacker, part, damage)
    let mut body_damage_events: Vec<(Entity, Entity, BodyPart, u8)> = Vec::new();

    for (attacker, combat_target, transform, lod, slot, attacker_eq) in &attacker_query {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let Some(target) = combat_target.0 else { continue };
        if target == attacker { continue; }

        if !health_query.contains(target) && !body_query.contains(target) {
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
            if let Ok(mut ai) = ai_query.get_mut(attacker) {
                ai.state = AiState::Attacking;
            }

            let mut damage = ATTACK_DAMAGE;
            if let Some(eq) = attacker_eq {
                if let Some(&weapon_ent) = eq.items.get(&EquipmentSlot::MainHand) {
                    if let Ok((Some(w_stats), _)) = item_stats_query.get(weapon_ent) {
                        damage += w_stats.damage_bonus;
                    }
                }
            }

            let target_part = BodyPart::random();

            if body_query.contains(target) {
                let mut mitigated_damage = damage;
                if let Ok(target_eq) = equipment_query.get(target) {
                    for (_slot, &armor_ent) in target_eq.items.iter() {
                        if let Ok((_, Some(a_stats))) = item_stats_query.get(armor_ent) {
                            if a_stats.covered_parts.contains(&target_part) {
                                let roll = fastrand::u8(0..100);
                                if roll < a_stats.coverage {
                                    mitigated_damage = mitigated_damage.saturating_sub(a_stats.damage_reduction);
                                }
                            }
                        }
                    }
                }
                body_damage_events.push((target, attacker, target_part, mitigated_damage.max(1)));
            } else {
                health_damage_events.push((target, attacker, damage));
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

    for (target, attacker, dmg) in health_damage_events {
        if let Ok(mut health) = health_query.get_mut(target) {
            health.current = health.current.saturating_sub(dmg);
        }
        if let Ok(Some(mut rel)) = rel_query.get_mut(target) {
            rel.update(attacker, -20);
        }
    }

    for (target, attacker, part, dmg) in body_damage_events {
        if let Ok(mut body) = body_query.get_mut(target) {
            let limb = body.get_mut(part);
            limb.current = limb.current.saturating_sub(dmg);
        }
        if let Ok(Some(mut rel)) = rel_query.get_mut(target) {
            rel.update(attacker, -20);
        }
    }
}

pub fn death_system(
    mut commands: Commands,
    mut registry: ResMut<FactionRegistry>,
    mut clock: ResMut<SimClock>,
    query: Query<(Entity, Option<&Health>, Option<&Body>, &Transform, Option<&FactionMember>, Option<&Person>, Option<&Wolf>, Option<&Deer>)>,
) {
    for (entity, health, body, transform, member, person, wolf, deer) in &query {
        let is_dead = match (health, body) {
            (Some(h), _) if h.is_dead() => true,
            (_, Some(b)) if b.is_dead() => true,
            _ => false,
        };
        if !is_dead { continue; }

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
