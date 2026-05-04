use crate::economy::goods::Good;
use crate::economy::item::{armor_coverage, Item};
use crate::simulation::animals::{AnimalAI, AnimalNeeds, AnimalState, Deer, Wolf};
use crate::simulation::corpse::{Corpse, CorpseMap, CorpseSpecies, CORPSE_FRESHNESS_TICKS};
use crate::simulation::construction::{Bed, HomeBed};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::items::{Equipment, EquipmentSlot, GroundItem};
use crate::simulation::line_of_sight::has_los;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::RelationshipMemory;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::stats::{self, Stats};
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

    /// Maps a body part to the matching `armor_coverage` flag used by
    /// `ArmorStats::covered_parts`. Left/right limbs share a flag because the
    /// coverage bitset only distinguishes head/torso/arms/legs.
    pub fn coverage_bit(self) -> u8 {
        match self {
            BodyPart::Head => armor_coverage::HEAD,
            BodyPart::Torso => armor_coverage::TORSO,
            BodyPart::LeftArm | BodyPart::RightArm => armor_coverage::ARMS,
            BodyPart::LeftLeg | BodyPart::RightLeg => armor_coverage::LEGS,
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

/// Fired when a Person is struck. Listeners (sound::respond_to_distress_system)
/// recruit nearby allies to defend. Throttled per-victim by `LastDistressEmit`.
#[derive(Event, Clone, Copy)]
pub struct DistressCallEvent {
    pub victim: Entity,
    pub attacker: Entity,
    pub tile: (i32, i32),
    pub z: i8,
    pub faction_id: u32,
}

/// Per-victim throttle so a series of cooldown-bounded swings doesn't re-run
/// the audible BFS every tick.
#[derive(Component, Clone, Copy, Default)]
pub struct LastDistressEmit(pub u64);

pub const DISTRESS_THROTTLE_TICKS: u64 = 20;

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CombatCooldown(pub f32);

const ATTACK_DAMAGE: u8 = 2;
const BASE_ATTACK_COOLDOWN: f32 = 1.0;

pub fn combat_system(
    time: Res<Time>,
    spatial: Res<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<crate::simulation::construction::DoorMap>,
    mut hand_drops: EventWriter<HandDropEvent>,
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
        Option<&mut crate::simulation::carry::Carrier>,
        Option<&Stats>,
    )>,
    mut health_query: Query<&mut Health>,
    mut body_query: Query<(&mut Body, Option<&Stats>)>,
    equipment_query: Query<&Equipment>,
    mut ai_query: Query<&mut PersonAI>,
    mut animal_ai_query: Query<(&mut AnimalAI, Option<&Deer>)>,
    mut rel_query: Query<Option<&mut RelationshipMemory>>,
    mut faction_registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    mut combat_events: EventWriter<CombatEvent>,
    mut discovery_events: EventWriter<crate::simulation::knowledge::DiscoveryActionEvent>,
) {
    let dt = time.delta_secs();

    // (target, attacker, damage)
    let mut health_damage_events: Vec<(Entity, Entity, u8)> = Vec::new();
    // (target, attacker, part, damage)
    let mut body_damage_events: Vec<(Entity, Entity, BodyPart, u8)> = Vec::new();
    // (faction_id) — attackers whose faction logs a combat event this frame
    let mut combat_activity_factions: Vec<u32> = Vec::new();
    // (attacker) — emitted as per-attacker DiscoveryActionEvent at end of system
    let mut combat_activity_attackers: Vec<Entity> = Vec::new();

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
        mut attacker_carrier,
        attacker_stats,
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

        // Free at least one hand for the swing; drop the heaviest stack to ground.
        if let Some(carrier) = attacker_carrier.as_mut() {
            if carrier.free_hands() == 0 {
                if let Some(stack) = carrier.drop_one_hand() {
                    let dtx = (transform.translation.x / TILE_SIZE).floor() as i32;
                    let dty = (transform.translation.y / TILE_SIZE).floor() as i32;
                    hand_drops.send(HandDropEvent {
                        tile: (dtx, dty),
                        good: stack.item.good,
                        qty: stack.qty,
                    });
                }
            }
        }

        let target_is_dead = if let Ok(h) = health_query.get(target) {
            h.is_dead()
        } else if let Ok((b, _)) = body_query.get(target) {
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
        // Attacker may be Person (tracks current_z) or animal (surface-only).
        let attacker_z = ai_query
            .get(attacker)
            .map(|ai| ai.current_z)
            .unwrap_or_else(|_| chunk_map.surface_z_at(tx, ty) as i8);

        let mut found = false;
        'find: for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                for &e in spatial.get(tx + dx, ty + dy) {
                    // Combat is adjacent — assume target is at the attacker's Z.
                    if e == target
                        && has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, attacker_z),
                            (tx + dx, ty + dy, attacker_z),
                        )
                    {
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

            let atk_dex = attacker_stats
                .map(|s| stats::modifier(s.dexterity))
                .unwrap_or(0);
            let tgt_dex = body_query
                .get(target)
                .ok()
                .and_then(|(_, s)| s)
                .map(|s| stats::modifier(s.dexterity))
                .unwrap_or(0);
            let hit_chance = (0.7 + 0.05 * (atk_dex - tgt_dex) as f32).clamp(0.2, 0.95);
            let attack_lands = fastrand::f32() < hit_chance;

            let mut damage = ATTACK_DAMAGE;
            let mut attack_speed_bonus = 1.0;

            if let Some(eq) = attacker_eq {
                if let Some(weapon) = eq.items.get(&EquipmentSlot::MainHand) {
                    if let Some(w_stats) = weapon.weapon_stats {
                        damage = damage.saturating_add(w_stats.damage_bonus);
                        attack_speed_bonus = w_stats.attack_speed();
                    }
                }
            }
            // STR adds melee damage; negative mods don't subtract from base damage.
            if let Some(s) = attacker_stats {
                damage = damage.saturating_add(stats::modifier(s.strength).max(0) as u8);
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
            combat_activity_attackers.push(attacker);

            // Apply cooldown
            if let Some(ref mut cd) = cd {
                cd.0 = BASE_ATTACK_COOLDOWN / attack_speed_bonus;
            }

            if !attack_lands {
                continue;
            }

            let target_part = BodyPart::random();

            if body_query.contains(target) {
                let mut mitigated_damage = damage;
                if let Ok(target_eq) = equipment_query.get(target) {
                    let part_bit = target_part.coverage_bit();
                    for armor in target_eq.items.values() {
                        let Some(a_stats) = armor.armor_stats else {
                            continue;
                        };
                        if !a_stats.covers(part_bit) {
                            continue;
                        }
                        let roll = fastrand::u8(0..100);
                        if roll < a_stats.coverage_pct {
                            mitigated_damage =
                                mitigated_damage.saturating_sub(a_stats.damage_reduction);
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
        if let Ok((_, mut target_combat, _, _, _, _, _, _, _, _, _)) =
            attacker_query.get_mut(target)
        {
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
        if let Ok((mut body, _)) = body_query.get_mut(target) {
            let limb = body.get_mut(part);
            limb.current = limb.current.saturating_sub(dmg);
        }
        if let Ok(Some(mut rel)) = rel_query.get_mut(target) {
            rel.update(attacker, -20);
        }

        // Retaliation
        if let Ok((_, mut target_combat, _, _, _, _, _, _, _, _, _)) =
            attacker_query.get_mut(target)
        {
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
    // Per-attacker discovery rolls (knowledge-system).
    for attacker in combat_activity_attackers {
        discovery_events.send(crate::simulation::knowledge::DiscoveryActionEvent {
            actor: attacker,
            activity: ActivityKind::Combat,
        });
    }
}

/// Sent when an agent drops a stack from their hands (combat reflex, etc).
/// `hand_drop_event_handler` consumes it.
#[derive(Event, Clone, Copy, Debug)]
pub struct HandDropEvent {
    pub tile: (i32, i32),
    pub good: Good,
    pub qty: u32,
}

pub fn hand_drop_event_handler(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut crate::simulation::items::GroundItem>,
    mut events: EventReader<HandDropEvent>,
) {
    for ev in events.read() {
        crate::simulation::items::spawn_or_merge_ground_item(
            &mut commands,
            &spatial,
            &mut ground_items,
            ev.tile.0,
            ev.tile.1,
            ev.good,
            ev.qty,
        );
    }
}

/// Reads `CombatEvent`s and emits a `DistressCallEvent` whenever a `Person` is
/// struck. Throttled per-victim by `LastDistressEmit` so a series of cooldown-
/// bounded swings doesn't re-run the audible BFS every tick. Lives in its own
/// system to keep `combat_system` under Bevy's 16-param ceiling.
pub fn distress_emit_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut combat_events: EventReader<CombatEvent>,
    mut distress_events: EventWriter<DistressCallEvent>,
    person_q: Query<
        (
            &Transform,
            &PersonAI,
            &FactionMember,
            Option<&Health>,
            Option<&Body>,
        ),
        With<Person>,
    >,
    last_emit_q: Query<&LastDistressEmit>,
) {
    for ev in combat_events.read() {
        let Ok((transform, ai, member, health, body)) = person_q.get(ev.target) else {
            continue;
        };
        if health.map_or(false, |h| h.is_dead()) || body.map_or(false, |b| b.is_dead()) {
            continue;
        }
        let last = last_emit_q.get(ev.target).map(|l| l.0).unwrap_or(0);
        if last != 0 && clock.tick.saturating_sub(last) < DISTRESS_THROTTLE_TICKS {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        distress_events.send(DistressCallEvent {
            victim: ev.target,
            attacker: ev.attacker,
            tile: (tx, ty),
            z: ai.current_z,
            faction_id: member.faction_id,
        });
        commands
            .entity(ev.target)
            .insert(LastDistressEmit(clock.tick));
    }
}

pub fn death_system(
    mut commands: Commands,
    mut registry: ResMut<FactionRegistry>,
    mut clock: ResMut<SimClock>,
    mut corpse_map: ResMut<CorpseMap>,
    mut bed_query: Query<&mut Bed>,
    query: Query<(
        Entity,
        Option<&Health>,
        Option<&Body>,
        &Transform,
        Option<&FactionMember>,
        Option<&Person>,
        Option<&Wolf>,
        Option<&Deer>,
        Option<&HomeBed>,
        Option<&crate::economy::agent::EconomicAgent>,
        Option<&crate::simulation::carry::Carrier>,
        Option<&Equipment>,
    )>,
) {
    for (
        entity,
        health,
        body,
        transform,
        member,
        person,
        wolf,
        deer,
        home_bed,
        agent,
        carrier,
        equipment,
    ) in &query
    {
        let is_dead = health.map_or(false, |h| h.is_dead()) || body.map_or(false, |b| b.is_dead());
        if !is_dead {
            continue;
        }

        if let Some(fm) = member {
            registry.remove_member(fm.faction_id);
        }

        // Release the bed claim so another agent can take it.
        if let Some(bed_entity) = home_bed.and_then(|h| h.0) {
            if let Ok(mut bed) = bed_query.get_mut(bed_entity) {
                bed.owner = None;
            }
        }
        if person.is_some() || wolf.is_some() || deer.is_some() {
            clock.population = clock.population.saturating_sub(1);
        }

        // Huntable animal: convert to a `Corpse` entity in place. No Meat/Skin
        // drops here — those come from butchering. Strip the AI/needs/species
        // markers so the corpse stops being processed by animal systems and
        // doesn't show up in prey scans.
        let huntable_species = if wolf.is_some() {
            Some(CorpseSpecies::Wolf)
        } else if deer.is_some() {
            Some(CorpseSpecies::Deer)
        } else {
            None
        };

        if let Some(species) = huntable_species {
            let tile = (
                (transform.translation.x / TILE_SIZE).floor() as i32,
                (transform.translation.y / TILE_SIZE).floor() as i32,
            );
            corpse_map.insert(tile, entity);
            let mut e = commands.entity(entity);
            e.remove::<Health>();
            e.remove::<Body>();
            e.remove::<AnimalAI>();
            e.remove::<AnimalNeeds>();
            e.remove::<Wolf>();
            e.remove::<Deer>();
            e.remove::<CombatTarget>();
            e.remove::<CombatCooldown>();
            e.remove::<crate::world::spatial::Indexed>();
            e.insert(Corpse {
                species,
                fresh_until_tick: clock.tick + CORPSE_FRESHNESS_TICKS,
            });
            // Animals carry no inventory/equipment, so nothing to spill.
            continue;
        }

        // Person death — spill inventory, hand-held loads, AND equipped items
        // as `GroundItem`s. Equipped items keep their material/quality (and
        // therefore their combat stats) so a looter can pick them back up and
        // re-equip them.
        let mut drops: Vec<(Item, u32)> = Vec::new();
        if let Some(a) = agent {
            for (item, qty) in &a.inventory {
                if *qty > 0 {
                    drops.push((*item, *qty));
                }
            }
        }
        if let Some(c) = carrier {
            for stack in [c.left, c.right].iter().flatten() {
                drops.push((stack.item, stack.qty));
            }
        }
        if let Some(eq) = equipment {
            for item in eq.items.values() {
                drops.push((*item, 1));
            }
        }

        for (item, qty) in drops {
            let mut loot_transform = *transform;
            loot_transform.translation.z = 0.3;
            commands.spawn((
                GroundItem { item, qty },
                loot_transform,
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
                crate::world::spatial::Indexed::new(
                    crate::world::spatial::IndexedKind::GroundItem,
                ),
            ));
        }

        commands.entity(entity).despawn_recursive();
    }
}
