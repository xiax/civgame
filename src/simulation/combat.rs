use crate::economy::item::{armor_coverage, Item};
use crate::simulation::animals::{AnimalAI, AnimalNeeds, AnimalState, Deer, Wolf};
use crate::simulation::sim_rng::{RngSite, SimRng};
use crate::simulation::construction::{Bed, HomeBed};
use crate::simulation::corpse::{Corpse, CorpseMap, CorpseSpecies, CORPSE_FRESHNESS_TICKS};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::items::{Equipment, EquipmentSlot, GroundItem};
use crate::simulation::line_of_sight::has_los;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::RelationshipMemory;
use crate::simulation::person::{AiState, Person, PersonAI};
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
    /// Deterministic hit-location pick from a caller-supplied local RNG.
    /// Combat systems build it from [`super::sim_rng::SimRng`] keyed on the
    /// acting entity + tick.
    pub fn random_from(rng: &mut fastrand::Rng) -> Self {
        let r = rng.u8(0..100);
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

    /// Dev/test convenience (no `SimRng` in scope). Production combat MUST use
    /// [`random_from`](Self::random_from).
    pub fn random() -> Self {
        Self::random_from(&mut fastrand::Rng::new())
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

/// Fired by `combat_system`'s retaliation branches when a victim's
/// `CombatTarget` is armed in response to a hit. `combat_system` cannot clear
/// the victim's `ActionQueue` directly: `attacker_query` already holds
/// `Option<&mut ActionQueue>` mutably across its iteration, so a sibling
/// `Query<&mut ActionQueue>` would alias. `combat_retaliation_cleanup_system`
/// drains this event in `Sequential` immediately after `combat_system` and
/// calls `aq.cancel_chain(&mut ai)` so the victim's stale plan doesn't carry
/// into the next dispatcher tick.
#[derive(Event, Clone, Copy)]
pub struct CombatRetaliationStartedEvent {
    pub victim: Entity,
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

// ── Ranged combat (plans/vehicle-system-tanks.md Phase 2) ─────────────────

/// Emitted by `combat_system` when a unit with a ranged weapon (`range > 1`)
/// strikes — instead of applying instant damage. `spawn_projectile_system`
/// turns it into a flying `Projectile`.
#[derive(Event, Clone, Copy, Debug)]
pub struct ProjectileFired {
    pub source: Entity,
    pub target: Entity,
    pub damage: u8,
    pub origin: Vec3,
    pub dest_tile: (i32, i32),
    /// Tiles per tick.
    pub speed: f32,
}

/// A projectile in flight. `progress` runs 0→1 over `origin`→`dest`; on
/// arrival `projectile_system` applies armor-mitigated damage and despawns.
#[derive(Component, Clone, Copy, Debug)]
pub struct Projectile {
    pub source: Entity,
    pub target: Entity,
    pub damage: u8,
    pub origin: Vec3,
    pub dest: Vec3,
    pub progress: f32,
    /// Fraction of the origin→dest span travelled per tick.
    pub step: f32,
}

/// Bundles `combat_system`'s three event writers into one `SystemParam`
/// to stay under Bevy's 16-parameter ceiling after retaliation cleanup
/// joined the system.
#[derive(bevy::ecs::system::SystemParam)]
pub struct CombatEventWriters<'w> {
    pub combat: EventWriter<'w, CombatEvent>,
    pub discovery: EventWriter<'w, crate::simulation::knowledge::DiscoveryActionEvent>,
    pub retaliation: EventWriter<'w, CombatRetaliationStartedEvent>,
    pub projectile: EventWriter<'w, ProjectileFired>,
    /// Per-suspect Sequential-set timing, folded here so `combat_system` stays
    /// under Bevy's 16-param ceiling. Read once via `events.timings.guard(..)`.
    pub timings: Res<'w, crate::simulation::speed::SuspectSystemTimings>,
    /// Deterministic sim RNG, folded here for the same 16-param-ceiling reason.
    /// Build local draws via `events.sim_rng.for_entity(..)`.
    pub sim_rng: Res<'w, SimRng>,
}

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
        Option<&FactionMember>,
        Option<&mut crate::simulation::carry::Carrier>,
        Option<&Stats>,
        Option<&mut crate::simulation::typed_task::ActionQueue>,
        Option<&mut crate::simulation::energy::Energy>,
    )>,
    mut health_query: Query<&mut Health>,
    mut body_query: Query<(&mut Body, Option<&Stats>)>,
    equipment_query: Query<&Equipment>,
    mut ai_query: Query<&mut PersonAI>,
    mut animal_ai_query: Query<(&mut AnimalAI, Option<&Deer>)>,
    mut rel_query: Query<Option<&mut RelationshipMemory>>,
    mut faction_registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    mut events: CombatEventWriters,
    vehicle_query: Query<(), With<crate::simulation::vehicle::Vehicle>>,
) {
    let _t = events
        .timings
        .guard(crate::simulation::speed::suspect::COMBAT);
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
        attacker_fm,
        mut attacker_carrier,
        attacker_stats,
        mut attacker_aq,
        mut attacker_energy,
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
                        resource_id: stack.item.resource_id,
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

        // A `Vehicle` target has no `Health` / `Body`; it is still a live
        // target — `vehicle_combat_system` (Sequential, after this) resolves
        // the per-cell damage. Don't clear the combat target here.
        if target_is_dead
            || (!health_query.contains(target)
                && !body_query.contains(target)
                && !vehicle_query.contains(target))
        {
            // Phase 5e-vii: drain the typed channel after a kill so a stale
            // `Task::Hunt { prey: <dead> }` doesn't linger in `aq.current`.
            // For non-hunt combat (Defend / brawl) `aq.current` is unrelated
            // and `advance()` is a no-op transition out of whatever else was
            // there — the next dispatcher tick re-establishes the right task.
            // Atomic ai + aq mutation via `finish_task` when both available;
            // otherwise the orphan invariant still holds because the attacker
            // can't both have a current task AND no PersonAI (Person spawns
            // attach both).
            if let Some(ref mut aq) = attacker_aq {
                if let Ok(mut ai) = ai_query.get_mut(attacker) {
                    aq.finish_task(&mut ai);
                } else {
                    aq.advance();
                }
            } else if let Ok(mut ai) = ai_query.get_mut(attacker) {
                ai.state = AiState::Idle;
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

        // Equipped weapon — a `range > 1` weapon widens acquisition and fires
        // a projectile instead of swinging.
        let weapon_stats = attacker_eq
            .and_then(|eq| eq.items.get(&EquipmentSlot::MainHand))
            .and_then(|w| w.weapon_stats);
        let weapon_range = weapon_stats.map(|w| w.range.max(1)).unwrap_or(1) as i32;

        let mut found = false;
        let mut found_tile = (tx, ty);
        'find: for dy in -weapon_range..=weapon_range {
            for dx in -weapon_range..=weapon_range {
                for &e in spatial.get(tx + dx, ty + dy) {
                    // Within reach (Chebyshev ≤ range) and with line of sight.
                    if e == target
                        && has_los(
                            &chunk_map,
                            &door_map,
                            (tx, ty, attacker_z),
                            (tx + dx, ty + dy, attacker_z),
                        )
                    {
                        found = true;
                        found_tile = (tx + dx, ty + dy);
                        break 'find;
                    }
                }
            }
        }

        if found {
            if let Ok(mut ai) = ai_query.get_mut(attacker) {
                // Combat overrides the action FSM without clearing the
                // current task — the retaliation cleanup system restores
                // it. Direct field write (no `aq` in this sub-query)
                // because we want the attacker's task to survive intact.
                ai.state = AiState::Attacking;
            }

            events.combat.send(CombatEvent { attacker, target });

            let atk_dex = attacker_stats
                .map(|s| stats::modifier(s.dexterity))
                .unwrap_or(0);
            let tgt_dex = body_query
                .get(target)
                .ok()
                .and_then(|(_, s)| s)
                .map(|s| stats::modifier(s.dexterity))
                .unwrap_or(0);
            let base_hit = 0.7 + 0.05 * (atk_dex - tgt_dex) as f32;
            // Ranged-only cover: partial-excavation rubble at the target tile
            // (level 1..=6) reduces hit chance by 5%/level, capped at 30%.
            // Melee is unaffected — a chebyshev-1 spear thrust doesn't get
            // meaningfully deflected by chipped rock under the target.
            let cover_pct = if weapon_range > 1 {
                let target_z = ai_query
                    .get(target)
                    .map(|ai| ai.current_z as i32)
                    .unwrap_or_else(|_| chunk_map.surface_z_at(found_tile.0, found_tile.1));
                let data = chunk_map.tile_at(found_tile.0, found_tile.1, target_z);
                let lvl = data.excavation_level();
                if lvl > 0 && lvl < 7 {
                    (lvl as f32 * 0.05).min(0.30)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let hit_chance = (base_hit - cover_pct).clamp(0.2, 0.95);
            let attack_lands = events
                .sim_rng
                .for_entity(attacker, clock.tick, RngSite::CombatHitRoll)
                .f32()
                < hit_chance;

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

            // Energy: a swing is tiring. When tired, the swing also lands
            // slower — lengthen the cooldown.
            let mut cooldown_scale = 1.0;
            if let Some(energy) = attacker_energy.as_deref_mut() {
                energy.drain(crate::simulation::energy::ENERGY_ATTACK_DRAIN);
                if energy.is_tired() {
                    cooldown_scale = 1.3;
                }
            }

            // Apply cooldown
            if let Some(ref mut cd) = cd {
                cd.0 = BASE_ATTACK_COOLDOWN / attack_speed_bonus * cooldown_scale;
            }

            if !attack_lands {
                continue;
            }

            // Ranged weapon: fire a projectile; damage resolves on arrival in
            // `projectile_system` (through the same armor-mitigation path).
            if let Some(ws) = weapon_stats.filter(|w| w.is_ranged()) {
                events.projectile.send(ProjectileFired {
                    source: attacker,
                    target,
                    damage,
                    origin: transform.translation,
                    dest_tile: found_tile,
                    speed: ws.projectile_speed().max(0.1),
                });
                continue;
            }

            let mut hit_rng =
                events
                    .sim_rng
                    .for_entity(attacker, clock.tick, RngSite::CombatArmorCoverage);
            let target_part = BodyPart::random_from(&mut hit_rng);

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
                        let roll = hit_rng.u8(0..100);
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
                    if let Some(ref mut aq) = attacker_aq {
                        aq.finish_task(&mut ai);
                    } else {
                        ai.state = AiState::Idle;
                    }
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
        if let Ok((_, mut target_combat, _, _, _, _, _, _, _, _, _, _)) =
            attacker_query.get_mut(target)
        {
            if let Ok(mut target_ai) = ai_query.get_mut(target) {
                // Combat-target assignment must happen exactly once.
                // The wake/cancel, however, must fire whenever a *sleeping*
                // victim is hit — even if they already have a combat target
                // (multi-attacker, or the 2nd of a body+health damage pair):
                // otherwise the deferred typed-task cancel is skipped and the
                // `Task::Sleep` is orphaned -> ActionQueue::dispatch desync.
                // Idempotent: after the wake `state != Sleeping`, so a later
                // hit won't re-send.
                let wake = target_combat.0.is_none() || target_ai.state == AiState::Sleeping;
                if target_combat.0.is_none() {
                    target_combat.0 = Some(attacker);
                }
                if wake {
                    // Setting target triggers combat_system next tick. The
                    // victim's ActionQueue is unreachable here (attacker_query
                    // holds it mutably); combat_retaliation_cleanup_system
                    // drains the event below and cancels the chain.
                    target_ai.state = AiState::Idle;
                    events
                        .retaliation
                        .send(CombatRetaliationStartedEvent { victim: target });
                }
            } else if let Ok((mut target_animal, maybe_deer)) = animal_ai_query.get_mut(target) {
                if target_combat.0.is_none() && maybe_deer.is_none() {
                    target_combat.0 = Some(attacker);
                    target_animal.target_entity = Some(attacker);
                    target_animal.state = AnimalState::Chase;
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
        if let Ok((_, mut target_combat, _, _, _, _, _, _, _, _, _, _)) =
            attacker_query.get_mut(target)
        {
            if let Ok(mut target_ai) = ai_query.get_mut(target) {
                // Same decoupling as the health-damage block above: wake a
                // sleeping victim (cancel the orphaned typed task) regardless
                // of whether they already have a combat target.
                let wake = target_combat.0.is_none() || target_ai.state == AiState::Sleeping;
                if target_combat.0.is_none() {
                    target_combat.0 = Some(attacker);
                }
                if wake {
                    target_ai.state = AiState::Idle;
                    events
                        .retaliation
                        .send(CombatRetaliationStartedEvent { victim: target });
                }
            } else if let Ok((mut target_animal, maybe_deer)) = animal_ai_query.get_mut(target) {
                if target_combat.0.is_none() && maybe_deer.is_none() {
                    target_combat.0 = Some(attacker);
                    target_animal.target_entity = Some(attacker);
                    target_animal.state = AnimalState::Chase;
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
        events
            .discovery
            .send(crate::simulation::knowledge::DiscoveryActionEvent {
                actor: attacker,
                activity: ActivityKind::Combat,
            });
    }
}

/// Drains `CombatRetaliationStartedEvent` and cancels the victim's typed-task
/// chain. Runs in `Sequential` after `combat_system`, where `attacker_query`'s
/// mutable borrow is released and a non-aliasing `&mut ActionQueue` is finally
/// reachable for the victim.
///
/// Uses `aq.cancel_chain(&mut ai)` (not `advance`) because combat onset
/// invalidates the entire pre-combat plan — every leg of the prefetch ring
/// belongs to the old goal. The next tick's `goal_update_system` will land on
/// `Defend` (or similar) and replan from scratch.
pub fn combat_retaliation_cleanup_system(
    mut events: EventReader<CombatRetaliationStartedEvent>,
    mut q: Query<(
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
    )>,
) {
    for ev in events.read() {
        if let Ok((mut ai, mut aq)) = q.get_mut(ev.victim) {
            aq.cancel_chain(&mut ai);
        }
    }
}

/// Turns `ProjectileFired` events into flying `Projectile` entities. Runs in
/// `Sequential` after `combat_system`.
pub fn spawn_projectile_system(mut commands: Commands, mut events: EventReader<ProjectileFired>) {
    for ev in events.read() {
        let dest_xy = crate::world::terrain::tile_to_world(ev.dest_tile.0, ev.dest_tile.1);
        let dest = Vec3::new(dest_xy.x, dest_xy.y, 0.6);
        let origin = Vec3::new(ev.origin.x, ev.origin.y, 0.6);
        let span = origin.distance(dest).max(TILE_SIZE);
        // `speed` is tiles/tick; convert to a fraction of the flight span.
        let step = (ev.speed * TILE_SIZE / span).clamp(0.05, 1.0);
        commands.spawn((
            Projectile {
                source: ev.source,
                target: ev.target,
                damage: ev.damage,
                origin,
                dest,
                progress: 0.0,
                step,
            },
            Sprite {
                color: Color::srgb(0.95, 0.9, 0.55),
                custom_size: Some(Vec2::splat(4.0)),
                ..default()
            },
            Transform::from_translation(origin),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ));
    }
}

/// Advances every `Projectile` along its origin→dest span; on arrival applies
/// armor-mitigated damage to the target (Person `Body`, plain `Health`, or a
/// bodiless `Vehicle` via `apply_vehicle_cell_damage`) and despawns. A target
/// that moved off the aimed tile, or despawned, is a clean miss.
#[allow(clippy::too_many_arguments)]
pub fn projectile_system(
    mut commands: Commands,
    mut projectiles: Query<(Entity, &mut Projectile, &mut Transform), Without<Person>>,
    mut health_query: Query<&mut Health>,
    mut body_query: Query<&mut Body>,
    equipment_query: Query<&Equipment>,
    target_tf_query: Query<&Transform, Without<Projectile>>,
    mut combat_targets: Query<&mut CombatTarget>,
    registry: Res<crate::simulation::vehicle::VehicleDesignRegistry>,
    mut vehicles: Query<(
        &mut crate::simulation::vehicle::Vehicle,
        &mut crate::simulation::vehicle::VehicleHealth,
    )>,
    clock: Res<SimClock>,
    sim_rng: Res<SimRng>,
) {
    for (proj_e, mut proj, mut tf) in projectiles.iter_mut() {
        proj.progress = (proj.progress + proj.step).min(1.0);
        tf.translation = proj.origin.lerp(proj.dest, proj.progress);
        if proj.progress < 1.0 {
            continue;
        }

        // Arrived. Resolve against the target if it is still on the aimed tile.
        let dest_tile = (
            (proj.dest.x / TILE_SIZE).floor() as i32,
            (proj.dest.y / TILE_SIZE).floor() as i32,
        );
        let on_tile = target_tf_query
            .get(proj.target)
            .map(|t| {
                let tx = (t.translation.x / TILE_SIZE).floor() as i32;
                let ty = (t.translation.y / TILE_SIZE).floor() as i32;
                (tx - dest_tile.0).abs().max((ty - dest_tile.1).abs()) <= 1
            })
            .unwrap_or(false);

        if on_tile {
            // Bodiless `Vehicle` target → per-cell damage.
            if let Ok((mut v, mut vhealth)) = vehicles.get_mut(proj.target) {
                let mut hit_cell_rng = sim_rng.for_entity(proj_e, clock.tick, RngSite::CombatMiscRoll);
                if let Some(hit) =
                    crate::simulation::vehicle::pick_hit_cell(&vhealth, &mut hit_cell_rng)
                {
                    if let Some(design) = registry.get(v.design_id).cloned() {
                        let out = crate::simulation::vehicle::apply_vehicle_cell_damage(
                            &mut vhealth,
                            &design,
                            hit,
                            proj.damage as u16,
                        );
                        if out.movement_disabled {
                            commands
                                .entity(proj.target)
                                .remove::<crate::simulation::vehicle::VehiclePathFollow>();
                            if v.state == crate::simulation::vehicle::VehicleState::Moving {
                                v.state = crate::simulation::vehicle::VehicleState::Parked;
                            }
                        }
                    }
                }
            } else if body_query.contains(proj.target) {
                // Keyed on the (unique) projectile entity + tick.
                let mut hit_rng =
                    sim_rng.for_entity(proj_e, clock.tick, RngSite::CombatArmorCoverage);
                let part = BodyPart::random_from(&mut hit_rng);
                let mut dmg = proj.damage;
                if let Ok(eq) = equipment_query.get(proj.target) {
                    let bit = part.coverage_bit();
                    for armor in eq.items.values() {
                        let Some(a) = armor.armor_stats else { continue };
                        if a.covers(bit) && hit_rng.u8(0..100) < a.coverage_pct {
                            dmg = dmg.saturating_sub(a.damage_reduction);
                        }
                    }
                }
                if let Ok(mut body) = body_query.get_mut(proj.target) {
                    let limb = body.get_mut(part);
                    limb.current = limb.current.saturating_sub(dmg.max(1));
                }
            } else if let Ok(mut health) = health_query.get_mut(proj.target) {
                health.current = health.current.saturating_sub(proj.damage);
            }

            // Retaliation — the struck target turns on the shooter.
            if let Ok(mut ct) = combat_targets.get_mut(proj.target) {
                if ct.0.is_none() {
                    ct.0 = Some(proj.source);
                }
            }
        }

        commands.entity(proj_e).despawn_recursive();
    }
}

/// Hunters whose prey is farther than this Chebyshev distance abandon the
/// chase. Keeps the dispatcher's next tick free to pick a closer target via
/// vision or fall through to `ScoutForPreyMethod` rather than chasing the
/// herd halfway across the map.
pub const HUNT_LEASH_RADIUS: i32 = 30;

/// Live re-targeting tick for `Task::Hunt { prey }`. Runs in
/// `SimulationSet::Sequential` between `movement_system` and `combat_system`.
///
/// Fixes two related bugs left over from the legacy `HuntFood` plan
/// migration:
///
/// 1. `assign_task_with_routing` caches `ai.dest_tile = prey_tile` at
///    dispatch time. When the prey flees (Deer/Horse/Cow flee on Person
///    sight) the hunter arrives at the now-stale tile, `movement_system`
///    flips state to `Working`, and `combat_system` can't find anything
///    adjacent — leaving the hunter frozen on the empty tile forever with
///    `aq.current == Task::Hunt { prey }` and `combat_target` still set.
///
/// 2. `combat_system`'s `Attacking → !found` branch aborts to Idle on the
///    first missed swing, so even a one-tile bump from the prey ends the
///    chase prematurely.
///
/// Strategy each tick:
/// - Look up the prey's live transform + Health.
/// - **Despawned / dead:** cancel the chain via `record_target_failure` so
///   `MethodHistory` biases the next dispatch (and the dispatcher's
///   next-tick re-eval picks a fresh prey or falls through to Scout).
/// - **Beyond `HUNT_LEASH_RADIUS`:** same — abandon, push `FailedTarget`.
/// - **Within Chebyshev 1:** no-op. `combat_system` handles the swing.
/// - **Moved ≥ 1 tile away:** point `dest_tile` / `target_tile` at the
///   prey's current tile, drop `Working → Seeking`, and reset
///   `PathFollow.segment_path` so `movement_system`'s `Following`-arm
///   replan re-routes through the path worker next tick.
pub fn hunt_chase_system(
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    prey_query: Query<(&Transform, &Health), Or<(With<Wolf>, With<Deer>)>>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut crate::simulation::htn::MethodHistory,
        &mut CombatTarget,
        &mut crate::pathfinding::path_request::PathFollow,
        &Transform,
        &LodLevel,
        &BucketSlot,
    )>,
) {
    let now = clock.tick;
    for (_entity, mut ai, mut aq, mut history, mut combat_target, mut pf, transform, lod, slot) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        let prey = match aq.current {
            crate::simulation::typed_task::Task::Hunt { prey } => prey,
            _ => continue,
        };

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        // Despawned, or dead, or no longer huntable: abandon. Defer cleanup
        // of `combat_target` + `aq.current` to the death-aware branch in
        // `combat_system` for the death case; the despawn case has to do it
        // here because combat_system early-returns when the entity is gone.
        let Ok((prey_t, prey_health)) = prey_query.get(prey) else {
            // Despawned. Cancel cleanly so dispatcher re-evaluates next tick.
            crate::simulation::htn::record_target_failure(&mut history, &mut ai, now);
            combat_target.0 = None;
            aq.cancel_chain(&mut ai);
            continue;
        };
        if prey_health.is_dead() {
            // combat_system's `target_is_dead` branch will handle the kill
            // bookkeeping in the same tick (it also calls aq.advance()).
            // Skip — don't double-cancel.
            continue;
        }

        let prey_tx = (prey_t.translation.x / TILE_SIZE).floor() as i32;
        let prey_ty = (prey_t.translation.y / TILE_SIZE).floor() as i32;

        let cheb = (cur_tx - prey_tx).abs().max((cur_ty - prey_ty).abs());

        // Already adjacent — combat_system will swing this tick.
        if cheb <= 1 {
            continue;
        }

        // Out of leash range. Abandon so the dispatcher can pick a closer
        // prey or transition the chief from Hunt → Scout. Stamping
        // FailedTarget on MethodHistory keeps the same prey from being
        // re-picked the next few ticks via the recency penalty.
        if cheb > HUNT_LEASH_RADIUS {
            crate::simulation::htn::record_target_failure(&mut history, &mut ai, now);
            combat_target.0 = None;
            aq.cancel_chain(&mut ai);
            continue;
        }

        // Re-target if the prey is no longer at the cached destination.
        // movement_system's `Following`-arm replan picks this up next tick
        // when it sees `pf.goal != goal3` derived from the updated target.
        if (ai.dest_tile.0 as i32, ai.dest_tile.1 as i32) != (prey_tx, prey_ty) {
            ai.dest_tile = (prey_tx, prey_ty);
            ai.target_tile = (prey_tx, prey_ty);
            ai.target_z = chunk_map.surface_z_at(prey_tx, prey_ty) as i8;
            // If we were already at the (now stale) dest tile, movement_system
            // had flipped us into Working. Drop back to Seeking so the
            // movement loop actually re-routes instead of accumulating
            // work_progress while the prey runs free.
            if ai.state == AiState::Working {
                let new_target = (prey_tx, prey_ty);
                let new_z = chunk_map.surface_z_at(prey_tx, prey_ty) as i8;
                aq.begin_seeking(&mut ai, new_target, new_z);
            }
            // Force a fresh path plan next tick. Mirror movement_system's
            // stuck-tick clear (lines 280-287) so the path worker rebuilds
            // against the live prey location.
            pf.segment_path.clear();
            pf.chunk_route.clear();
            pf.segment_cursor = 0;
            pf.route_cursor = 0;
            pf.status = crate::pathfinding::path_request::FollowStatus::Idle;
        }
    }
}

/// Sent when an agent drops a stack from their hands (combat reflex, etc).
/// `hand_drop_event_handler` consumes it.
#[derive(Event, Clone, Copy, Debug)]
pub struct HandDropEvent {
    pub tile: (i32, i32),
    pub resource_id: crate::economy::resource_catalog::ResourceId,
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
            ev.resource_id,
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
        Option<&crate::simulation::animals::PackAnimalInventory>,
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
        pack_inv,
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
            // P8: tamed pack animals (Horse/Cow/Pig that became Wolf/Deer
            // corpses don't apply here, but stash for the non-huntable
            // tamed branch below). Wolves/Deer never carry packs.
            continue;
        }

        // P8: tamed pack animals (Horse/Cow/Pig) drop their pack inventory
        // as `GroundItem`s on death. These don't go through the Wolf/Deer
        // huntable branch above — they fall through and get cleanly
        // despawned alongside their cargo here.
        if let Some(inv) = pack_inv {
            for (rid, qty) in inv.iter() {
                if qty == 0 {
                    continue;
                }
                let mut loot_transform = *transform;
                loot_transform.translation.z = 0.3;
                commands.spawn((
                    GroundItem {
                        item: Item::new_commodity(rid),
                        qty,
                        owner_household: None,
                        spawned_tick: 0,
                    },
                    loot_transform,
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    crate::world::spatial::Indexed::new(
                        crate::world::spatial::IndexedKind::GroundItem,
                    ),
                ));
            }
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
                GroundItem {
                    item,
                    qty,
                    owner_household: None,
                    spawned_tick: 0,
                },
                loot_transform,
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
                crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
            ));
        }

        commands.entity(entity).despawn_recursive();
    }
}
