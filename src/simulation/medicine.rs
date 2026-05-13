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
use crate::simulation::schedule::SimClock;

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
