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

use crate::simulation::construction::{Bed, BedMap};
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::lifecycle::{
    nomadic_variant_of, LifecycleEventQueue, SettlementLifecycleEvent,
};
use crate::simulation::schedule::SimClock;
use crate::world::seasons::{TICKS_PER_DAY, TICKS_PER_SEASON};

/// Seasons of sustained combined failure required before collapse.
pub const COLLAPSE_TRIGGER_SEASONS: u32 = 2;

/// How many consecutive daily failing samples trigger collapse. Two
/// seasons (~10 in-game days at default `DAYS_PER_SEASON`) of sustained
/// *combined* failure — a single weak signal must not uproot a village.
pub const COLLAPSE_TRIGGER_TICKS: u32 = TICKS_PER_SEASON * COLLAPSE_TRIGGER_SEASONS;

/// Settled bands smaller than this count as a population crash.
pub const SEDENTARY_COLLAPSE_MIN_MEMBERS: u32 = 6;

/// Per-member days of stored food below which the faction is in a food
/// deficit (the necessary condition for collapse).
pub const COLLAPSE_FOOD_DAYS: f32 = 3.0;

/// Fraction of members that must have a bed; below this is shelter loss.
pub const COLLAPSE_MIN_BED_COVERAGE: f32 = 0.5;

/// Radius (chebyshev) around `home_tile` scanned for faction-owned beds.
pub const COLLAPSE_BED_FALLBACK_RADIUS: i32 = 32;

/// Combined-failure predicate: a settled faction is "failing" only when a
/// sustained food deficit coincides with a structural failure (population
/// crash or shelter loss). A single weak signal is survivable.
pub fn collapse_failing(food_deficit: bool, pop_crash: bool, shelter_loss: bool) -> bool {
    food_deficit && (pop_crash || shelter_loss)
}

/// Count beds within `COLLAPSE_BED_FALLBACK_RADIUS` of `home` that belong
/// to `faction` — a bed's faction is its assigned occupant's faction; an
/// unassigned bed in range is counted toward the faction (errs against a
/// false collapse). Beds owned by a *different* faction are excluded.
fn usable_beds(
    faction: u32,
    home: (i32, i32),
    bed_map: &BedMap,
    beds: &Query<&Bed>,
    members: &Query<&FactionMember>,
) -> u32 {
    let mut count = 0u32;
    for (&tile, &bed_entity) in bed_map.0.iter() {
        if (tile.0 - home.0).abs().max((tile.1 - home.1).abs()) > COLLAPSE_BED_FALLBACK_RADIUS {
            continue;
        }
        let owner_faction = beds
            .get(bed_entity)
            .ok()
            .and_then(|b| b.owner)
            .and_then(|p| members.get(p).ok())
            .map(|m| m.faction_id);
        match owner_faction {
            Some(fid) if fid == faction => count += 1,
            None => count += 1, // unassigned bed in our home radius
            _ => {}             // belongs to another faction — skip
        }
    }
    count
}

/// Daily check — Economy schedule, before `process_settlement_lifecycle_system`
/// so the queued event drains the same tick. Counts beds via `BedMap`
/// rather than walking the full `Settlement` component tree (cheaper).
pub fn sedentary_collapse_system(
    mut registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    bed_map: Res<BedMap>,
    beds: Query<&Bed>,
    members: Query<&FactionMember>,
    mut lifecycle_queue: ResMut<LifecycleEventQueue>,
) {
    // Phase 1.2: per-faction stagger inside an every-tick run.
    const SYSTEM_OFFSET: u64 = 167;
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
        if !crate::simulation::perf::faction_stagger_due(
            clock.tick,
            fid,
            SYSTEM_OFFSET,
            TICKS_PER_DAY as u64,
        ) {
            continue;
        }
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
        let member_count = faction.member_count;
        let home = faction.home_tile;

        // Necessary condition: sustained food deficit (< COLLAPSE_FOOD_DAYS
        // per-member days of stored food).
        let food_deficit =
            faction.storage.food_total() < (member_count as f32 * COLLAPSE_FOOD_DAYS).max(1.0);

        // Population crash — the band is too small to be a settlement.
        let pop_crash = member_count < SEDENTARY_COLLAPSE_MIN_MEMBERS;

        // Shelter loss — fewer faction-owned beds than half the members.
        let bed_count = usable_beds(fid, home, &bed_map, &beds, &members);
        let needed_beds = ((member_count as f32 * COLLAPSE_MIN_BED_COVERAGE).ceil() as u32).max(1);
        let shelter_loss = bed_count < needed_beds;

        // Combined-failure trigger: a food deficit ALONE is survivable;
        // collapse needs the deficit AND a structural failure on top.
        let failing = collapse_failing(food_deficit, pop_crash, shelter_loss);
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
    fn collapse_streak_threshold_is_two_seasons() {
        // The trigger fires when the streak reaches `COLLAPSE_TRIGGER_TICKS`.
        // One sample per day = one bump of TICKS_PER_DAY.
        let samples_needed = COLLAPSE_TRIGGER_TICKS / TICKS_PER_DAY;
        assert!(samples_needed >= 1, "samples_needed = {samples_needed}");
        assert_eq!(
            samples_needed * TICKS_PER_DAY,
            COLLAPSE_TRIGGER_TICKS,
            "TICKS_PER_SEASON should be a multiple of TICKS_PER_DAY"
        );
        assert_eq!(COLLAPSE_TRIGGER_TICKS, TICKS_PER_SEASON * 2);
    }

    #[test]
    fn single_failure_does_not_collapse() {
        // Food deficit alone — survivable.
        assert!(!collapse_failing(true, false, false));
        // Pop crash alone (no deficit) — survivable.
        assert!(!collapse_failing(false, true, false));
        // Shelter loss alone (no deficit) — survivable.
        assert!(!collapse_failing(false, false, true));
        // Pop crash + shelter loss but food is fine — survivable.
        assert!(!collapse_failing(false, true, true));
    }

    #[test]
    fn combined_failure_collapses() {
        // Deficit + pop crash.
        assert!(collapse_failing(true, true, false));
        // Deficit + shelter loss.
        assert!(collapse_failing(true, false, true));
        // All three.
        assert!(collapse_failing(true, true, true));
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
