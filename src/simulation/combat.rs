use crate::economy::goods::Good;
use crate::economy::item::Item;
use crate::simulation::animals::{AnimalAI, AnimalState, Deer, Wolf};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::items::{ArmorStats, Equipment, EquipmentSlot, GroundItem, WeaponStats};
use crate::simulation::line_of_sight::has_los;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::RelationshipMemory;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::technology::ActivityKind;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use bevy::prelude::*;

#[derive(Component, Clone, Copy, Debug)]
pub struct Health {
    pub current: u8,
    pub max: u8,
}

impl Health {
    pub fn new(max: u8) -> Self {
        Self { current: max, max }
    }
    pub fn is_dead(self) -> bool {
        self.current == 0
    }
    pub fn fraction(self) -> f32 {
        self.current as f32 / self.max as f32
    }
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
        BodyPart::Head,
        BodyPart::Torso,
        BodyPart::LeftArm,
        BodyPart::RightArm,
        BodyPart::LeftLeg,
        BodyPart::RightLeg,
    ];
    pub fn random() -> Self {
        let r = fastrand::u8(0..100);
        if r < 10 {
            BodyPart::Head
        } else if r < 50 {
            BodyPart::Torso
        } else if r < 62 {
            BodyPart::LeftArm
        } else if r < 74 {
            BodyPart::RightArm
        } else if r < 87 {
            BodyPart::LeftLeg
        } else {
            BodyPart::RightLeg
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LimbHealth {
    pub current: u8,
    pub max: u8,
}

impl LimbHealth {
    pub fn new(max: u8) -> Self {
        Self { current: max, max }
    }
    pub fn is_destroyed(&self) -> bool {
        self.current == 0
    }
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
        self.parts[BodyPart::Head as usize].is_destroyed()
            || self.parts[BodyPart::Torso as usize].is_destroyed()
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

#[derive(Event)]
pub struct CombatEvent {
    pub attacker: Entity,
    pub target: Entity,
}

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CombatCooldown(pub f32);

const ATTACK_DAMAGE: u8 = 2;
const BASE_ATTACK_COOLDOWN: f32 = 1.0;

pub fn combat_system(
    time: Res<Time>,
    spatial: Res<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    mut attacker_query: Query<(
        Entity,
        &mut CombatTarget,
        &Transform,
        &LodLevel,
        &BucketSlot,
        Option<&Equipment>,
        Option<&mut CombatCooldown>,
        Option<&mut ActivePlan>,
        Option<&FactionMember>,
    )>,
    mut health_query: Query<&mut Health>,
    mut body_query: Query<&mut Body>,
    equipment_query: Query<&Equipment>,
    item_stats_query: Query<(Option<&WeaponStats>, Option<&ArmorStats>)>,
    mut ai_query: Query<&mut PersonAI>,
    mut animal_ai_query: Query<(&mut AnimalAI, Option<&Deer>)>,
    mut rel_query: Query<Option<&mut RelationshipMemory>>,
    mut faction_registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    mut combat_events: EventWriter<CombatEvent>,
) {
    let dt = time.delta_secs();

    // (target, attacker, damage)
    let mut health_damage_events: Vec<(Entity, Entity, u8)> = Vec::new();
    // (target, attacker, part, damage)
    let mut body_damage_events: Vec<(Entity, Entity, BodyPart, u8)> = Vec::new();
    // (faction_id) — attackers whose faction logs a combat event this frame
    let mut combat_activity_factions: Vec<u32> = Vec::new();

    for (
        attacker,
        mut combat_target,
        transform,
        lod,
        slot,
        attacker_eq,
        mut cd,
        _active_plan_opt,
        attacker_fm,
    ) in &mut attacker_query
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        if let Some(ref mut cd) = cd {
            cd.0 = (cd.0 - dt * clock.scale_factor()).max(0.0);
            if cd.0 > 0.0 {
                continue;
            }
        }

        let Some(target) = combat_target.0 else {
            continue;
        };
        if target == attacker {
            continue;
        }

        let target_is_dead = if let Ok(h) = health_query.get(target) {
            h.is_dead()
        } else if let Ok(b) = body_query.get(target) {
            b.is_dead()
        } else {
            false
        };

        if target_is_dead || (!health_query.contains(target) && !body_query.contains(target)) {
            if let Ok(mut ai) = ai_query.get_mut(attacker) {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
            if let Ok((mut animal_ai, _)) = animal_ai_query.get_mut(attacker) {
                animal_ai.state = AnimalState::Wander;
                animal_ai.target_entity = None;
            }
            combat_target.0 = None;
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut found = false;
        'find: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                for &e in spatial.get(tx + dx, ty + dy) {
                    if e == target && has_los(&chunk_map, (tx, ty), (tx + dx, ty + dy)) {
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

            combat_events.send(CombatEvent { attacker, target });

            let mut damage = ATTACK_DAMAGE;
            let mut attack_speed_bonus = 1.0;

            if let Some(eq) = attacker_eq {
                if let Some(&weapon_ent) = eq.items.get(&EquipmentSlot::MainHand) {
                    if let Ok((Some(w_stats), _)) = item_stats_query.get(weapon_ent) {
                        damage += w_stats.damage_bonus;
                        attack_speed_bonus = w_stats.attack_speed;
                    }
                }
            }
            // Apply faction tech combat bonus
            if let Some(fm) = attacker_fm {
                if fm.faction_id != SOLO {
                    if let Some(fd) = faction_registry.factions.get(&fm.faction_id) {
                        damage = damage.saturating_add(fd.combat_damage_bonus());
                    }
                    combat_activity_factions.push(fm.faction_id);
                }
            }

            // Apply cooldown
            if let Some(ref mut cd) = cd {
                cd.0 = BASE_ATTACK_COOLDOWN / attack_speed_bonus;
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
                                    mitigated_damage =
                                        mitigated_damage.saturating_sub(a_stats.damage_reduction);
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
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
            }
        }
    }

    // Process damage and Retaliation
    for (target, attacker, dmg) in health_damage_events {
        if let Ok(mut health) = health_query.get_mut(target) {
            health.current = health.current.saturating_sub(dmg);
        }
        if let Ok(Some(mut rel)) = rel_query.get_mut(target) {
            rel.update(attacker, -20);
        }

        // Retaliation
        if let Ok((_, mut target_combat, _, _, _, _, _, _, _)) = attacker_query.get_mut(target) {
            if target_combat.0.is_none() {
                if let Ok(mut target_ai) = ai_query.get_mut(target) {
                    target_combat.0 = Some(attacker);
                    // Setting target will trigger combat_system on next tick
                    target_ai.state = AiState::Idle;
                } else if let Ok((mut target_animal, maybe_deer)) = animal_ai_query.get_mut(target)
                {
                    if maybe_deer.is_none() {
                        target_combat.0 = Some(attacker);
                        target_animal.target_entity = Some(attacker);
                        target_animal.state = AnimalState::Chase;
                    }
                }
            }
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

        // Retaliation
        if let Ok((_, mut target_combat, _, _, _, _, _, _, _)) = attacker_query.get_mut(target) {
            if target_combat.0.is_none() {
                if let Ok(mut target_ai) = ai_query.get_mut(target) {
                    target_combat.0 = Some(attacker);
                    target_ai.state = AiState::Idle;
                } else if let Ok((mut target_animal, maybe_deer)) = animal_ai_query.get_mut(target)
                {
                    if maybe_deer.is_none() {
                        target_combat.0 = Some(attacker);
                        target_animal.target_entity = Some(attacker);
                        target_animal.state = AnimalState::Chase;
                    }
                }
            }
        }
    }

    // Record combat activity for attacking factions
    for faction_id in combat_activity_factions {
        if let Some(fd) = faction_registry.factions.get_mut(&faction_id) {
            fd.activity_log.increment(ActivityKind::Combat);
        }
    }
}

pub fn death_system(
    mut commands: Commands,
    mut registry: ResMut<FactionRegistry>,
    mut clock: ResMut<SimClock>,
    query: Query<(
        Entity,
        Option<&Health>,
        Option<&Body>,
        &Transform,
        Option<&FactionMember>,
        Option<&Person>,
        Option<&Wolf>,
        Option<&Deer>,
    )>,
) {
    for (entity, health, body, transform, member, person, wolf, deer) in &query {
        let is_dead = health.map_or(false, |h| h.is_dead()) || body.map_or(false, |b| b.is_dead());
        if !is_dead {
            continue;
        }

        if let Some(fm) = member {
            registry.remove_member(fm.faction_id);
        }
        if person.is_some() || wolf.is_some() || deer.is_some() {
            clock.population = clock.population.saturating_sub(1);
        }

        let drops: Vec<(Good, u32)> = if wolf.is_some() {
            vec![(Good::Meat, 3), (Good::Skin, 1)]
        } else if deer.is_some() {
            vec![(Good::Meat, 5), (Good::Skin, 2)]
        } else {
            vec![]
        };

        for (good, qty) in drops {
            let mut loot_transform = *transform;
            loot_transform.translation.z = 0.3;
            commands.spawn((
                GroundItem {
                    item: Item::new_commodity(good),
                    qty,
                },
                loot_transform,
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        }

        commands.entity(entity).despawn_recursive();
    }
}
