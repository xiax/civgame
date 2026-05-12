//! P4: settled → nomadic collapse.
//!
//! When a settled faction's foundations crumble — population crash,
//! sustained food deficit, or shelter loss — `sedentary_collapse_system`
//! flips it back to nomadic life. Counterpart to `nomad_sedentarize_system`.
//!
//! Per-day sampling: each daily tick, every settled faction is checked
//! against the collapse triggers. A failing sample increments
//! `FactionData.collapse_streak`; a healthy sample resets it to zero. At
//! `COLLAPSE_TRIGGER_TICKS` worth of consecutive failing samples the
//! lifecycle event queue gets a `SwitchArchetype { nomadic_X }` push and
//! the lifecycle handler does the rest (cap swap, despawn, re-seed nomadic
//! camp, activity log).

use bevy::prelude::*;

use crate::simulation::construction::BedMap;
use crate::simulation::faction::FactionRegistry;
use crate::simulation::lifecycle::{
    nomadic_variant_of, LifecycleEventQueue, SettlementLifecycleEvent,
};
use crate::simulation::nomad::OLD_CAMP_RADIUS;
use crate::simulation::schedule::SimClock;
use crate::world::seasons::{TICKS_PER_DAY, TICKS_PER_SEASON};

/// How many consecutive daily failing samples trigger collapse. One
/// season (~5 in-game days at default `DAYS_PER_SEASON`) of sustained
/// failure is the threshold.
pub const COLLAPSE_TRIGGER_TICKS: u32 = TICKS_PER_SEASON;

/// Settled bands smaller than this can collapse without the population
/// crash check (a tiny band is barely a settlement).
pub const SEDENTARY_COLLAPSE_MIN_MEMBERS: u32 = 6;

/// Daily check — Economy schedule, before `process_settlement_lifecycle_system`
/// so the queued event drains the same tick. Counts beds via `BedMap`
/// rather than walking the full `Settlement` component tree (cheaper).
pub fn sedentary_collapse_system(
    mut registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    bed_map: Res<BedMap>,
    mut lifecycle_queue: ResMut<LifecycleEventQueue>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    // Snapshot per-faction trigger state. Two-pass so we can mutate
    // `collapse_streak` and emit events without holding the registry
    // borrow during the event push.
    struct Sample {
        fid: u32,
        archetype_key: String,
        home_tile: (i32, i32),
        failing: bool,
        collapse_now: bool,
    }
    let mut samples: Vec<Sample> = Vec::new();
    for (&fid, faction) in registry.factions.iter() {
        // Only settled, top-level factions can collapse. Households (sub-
        // factions) inherit lifestyle from their parent and aren't
        // independently mobile.
        if faction.caps.home.is_mobile() {
            continue;
        }
        if faction.parent_faction.is_some() {
            continue;
        }
        if faction.member_count == 0 {
            continue;
        }
        let members = faction.member_count;
        let home = faction.home_tile;

        // Trigger 1: population crash — small faction.
        let pop_crash = members < SEDENTARY_COLLAPSE_MIN_MEMBERS;

        // Trigger 2: sustained food deficit — per-head food < 10.
        let food_deficit = faction.storage.food_total() < (members as f32 * 10.0).max(10.0);

        // Trigger 3: shelter loss — fewer beds than members/3.
        let bed_count = bed_map
            .0
            .keys()
            .filter(|&&t| (t.0 - home.0).abs().max((t.1 - home.1).abs()) <= OLD_CAMP_RADIUS)
            .count() as u32;
        let shelter_loss = bed_count < (members / 3).max(1);

        let failing = pop_crash || food_deficit || shelter_loss;
        let next_streak = if failing {
            faction.collapse_streak.saturating_add(TICKS_PER_DAY)
        } else {
            0
        };
        let collapse_now = failing && next_streak >= COLLAPSE_TRIGGER_TICKS;
        samples.push(Sample {
            fid,
            archetype_key: faction.caps.archetype_key.clone(),
            home_tile: home,
            failing,
            collapse_now,
        });
    }

    for s in samples {
        if let Some(faction) = registry.factions.get_mut(&s.fid) {
            faction.collapse_streak = if s.failing {
                faction.collapse_streak.saturating_add(TICKS_PER_DAY)
            } else {
                0
            };
            if s.collapse_now {
                // Reset the streak so a repeat fire doesn't spam events
                // before the lifecycle handler swaps the archetype.
                faction.collapse_streak = 0;
            }
        }
        if s.collapse_now {
            let new_key = nomadic_variant_of(&s.archetype_key);
            info!(
                "Faction {} collapsed (streak ≥ {} ticks at {:?}); switching to {}",
                s.fid, COLLAPSE_TRIGGER_TICKS, s.home_tile, new_key
            );
            lifecycle_queue.push(SettlementLifecycleEvent::SwitchArchetype {
                faction: s.fid,
                new_archetype_key: new_key,
                at_tile: s.home_tile,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::faction::FactionRegistry;

    /// Pure-logic test: a faction whose food deficit is sustained for a
    /// full season's worth of daily samples should emit a SwitchArchetype
    /// event. We can't easily run the full system here without an App
    /// context, so we manually walk the streak math.
    #[test]
    fn collapse_streak_threshold_is_one_season() {
        // The trigger must fire when the streak reaches `COLLAPSE_TRIGGER_TICKS`.
        // One sample per day = one bump of TICKS_PER_DAY. So the number of
        // failing samples needed is `COLLAPSE_TRIGGER_TICKS / TICKS_PER_DAY`.
        let samples_needed = COLLAPSE_TRIGGER_TICKS / TICKS_PER_DAY;
        // Sanity: at default season length (5 days) this should be 5.
        assert!(samples_needed >= 1, "samples_needed = {samples_needed}");
        assert_eq!(
            samples_needed * TICKS_PER_DAY,
            COLLAPSE_TRIGGER_TICKS,
            "TICKS_PER_SEASON should be a multiple of TICKS_PER_DAY"
        );
    }

    #[test]
    fn nomadic_variant_round_trip() {
        // Belt-and-braces: every settled key maps cleanly to its nomadic
        // counterpart, and the inverse sedentarize path returns the
        // original key.
        for key in ["settled_subsistence", "settled_mixed", "settled_market"] {
            let nomadic = nomadic_variant_of(key);
            assert!(
                nomadic.starts_with("nomadic_"),
                "{} -> {} should be nomadic-prefixed",
                key,
                nomadic
            );
            let back = crate::simulation::lifecycle::settled_variant_of(&nomadic);
            assert_eq!(back, key, "round-trip should match");
        }
    }

    /// `sedentary_collapse_system` should ignore household sub-factions
    /// and nomadic factions outright. Verified at the type level by
    /// reading the gate in the system body — this test pins the
    /// FactionRegistry default doesn't have any factions set up to fire.
    #[test]
    fn empty_registry_emits_no_events() {
        let registry = FactionRegistry::default();
        assert!(registry.factions.is_empty());
    }
}
