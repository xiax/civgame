//! Ambient work-social multitasking.
//!
//! Coworkers laboring side-by-side gain *secondary* social interaction
//! (relationships, awareness / settlement / wage gossip, knowledge-tier
//! promotion) at the same strength as a dedicated `AgentGoal::Socialize`
//! session, **without** abandoning their primary work — `ActionQueue` /
//! `JobClaim` / routing / inventory / work-progress stay single-owner.
//!
//! `ambient_social_pairing_system` (ParallelA, after `tick_needs_system`,
//! before `goal_update_system` / `opportunistic_interrupt_system`) stamps a
//! data-only [`SecondarySocial`] marker on every agent doing compatible
//! primary work near a same-root-faction coworker. The five social consumer
//! systems then gate on the shared [`is_social_contact`] predicate
//! (`dedicated Socialize  OR  live SecondarySocial`) instead of the exclusive
//! `AgentGoal::Socialize`, keeping their existing all-neighbor scans and
//! per-tick effect rates verbatim.
//!
//! Deliberate mastery transfer (`knowledge::tech_teaching_system`) is
//! **intentionally not** routed through this predicate — casual work chatter
//! must not become accidental instruction.

use crate::simulation::faction::{FactionRegistry, SOCIAL_RADIUS, SOLO};
use crate::simulation::goals::{is_maintenance_goal, AgentGoal, RescueTarget};
use crate::simulation::lod::LodLevel;
use crate::simulation::nomad::MigrationTarget;
use crate::simulation::nomad_pack_labor::PackingDuty;
use crate::simulation::person::{Drafted, Person};
use crate::simulation::schedule::SimClock;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use bevy::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocialMode {
    /// Secondary, work-piggybacked. Dedicated `AgentGoal::Socialize` is
    /// goal-driven and carries **no** component (never double-counted).
    Ambient,
}

/// Per-agent "socially-open-while-working" marker.
///
/// **Always present on every Person from spawn** (added to every spawn
/// bundle, default [`SecondarySocial::inactive`]). The pairing system only
/// ever *mutates* it — it is never inserted/removed at runtime. This is
/// deliberate: insert/remove churns the entity's archetype every rescan,
/// which globally reorders `Query` iteration, and the construction/seeding
/// layout is (fragilely) sensitive to Person iteration order. A uniform
/// always-present component keeps every Person in one stable archetype, so
/// iteration order matches the pre-feature baseline.
///
/// `partner` is the deterministic nearest valid coworker — the witness that
/// ≥1 valid coworker existed at pairing time (so lone workers don't get
/// spurious social relief) and for the inspector / tests. **Consumers ignore
/// `partner`**: they keep their existing all-neighbor 3-tile scan; the
/// reduction vs. dedicated Socialize is structural (the two-sided snapshot
/// gate still requires both ends socially active), not weighted.
/// `partner == None` (or `expires_tick <= now`) ⇒ inactive.
#[derive(Component, Clone, Copy, Debug)]
pub struct SecondarySocial {
    pub partner: Option<Entity>,
    pub mode: SocialMode,
    pub expires_tick: u32,
}

impl SecondarySocial {
    /// The not-paired resting state every Person carries until the pairing
    /// system stamps a live partner onto it.
    pub const fn inactive() -> Self {
        Self {
            partner: None,
            mode: SocialMode::Ambient,
            expires_tick: 0,
        }
    }

    #[inline]
    pub fn is_active(&self, now: u32) -> bool {
        self.partner.is_some() && self.expires_tick > now
    }
}

/// Game-time-scaled pairing persistence window: ~20 s @ 20 Hz.
/// `> 2× TIER_PROMOTION_CADENCE (200)` so an ambient pair survives long
/// enough for at least one tier-promotion sample, `<< TICKS_PER_DAY (3600)`
/// so a stale pair self-expires the same game-day.
pub const PAIRING_WINDOW: u32 = 400;

/// Per-agent re-scan cadence for `ambient_social_pairing_system`. Cleanup /
/// expiry runs every tick (cheap); the `O(SOCIAL_RADIUS²)` partner scan runs
/// once per this many ticks per agent, staggered by entity index so the
/// scans spread across ticks. `PAIRING_WINDOW` covers ≥ 3 rescans so a
/// still-valid pair is refreshed long before it expires.
#[cfg(not(test))]
pub const PAIRING_RESCAN_CADENCE: u64 = 100;
/// Shrunk under test so integration fixtures need only a few ticks.
#[cfg(test)]
pub const PAIRING_RESCAN_CADENCE: u64 = 5;

/// Pure, unit-testable. True iff the agent is doing compatible *primary
/// work* and is in no incompatible state, so it may **also** carry an
/// ambient social contact. Rejects the dedicated-Socialize path (that uses
/// the goal pipeline, not this component) and all maintenance / combat /
/// migration / drafted / dormant / SOLO states.
///
/// `AiState` is deliberately NOT consulted: the work-goal set already
/// encodes "doing primary work", and gating on `Working/Seeking/Routing`
/// would drop the pairing during the Idle blip between a gather and its
/// deposit leg every chain cycle, for no real benefit.
#[allow(clippy::too_many_arguments)]
pub fn is_ambient_social_compatible(
    goal: AgentGoal,
    lod: LodLevel,
    faction_id: u32,
    drafted: bool,
    in_combat: bool,
    raiding: bool,
    rescuing: bool,
    migrating: bool,
    packing: bool,
) -> bool {
    if faction_id == SOLO {
        return false;
    }
    if lod == LodLevel::Dormant {
        return false;
    }
    if drafted || in_combat || raiding || rescuing || migrating || packing {
        return false;
    }
    // Dedicated Socialize is goal-driven; never stamp a SecondarySocial on
    // it (would double-count in the consumers).
    if matches!(goal, AgentGoal::Socialize) {
        return false;
    }
    // Maintenance goals (Survive/Drink/Sleep/SeekCare) are not primary work.
    if is_maintenance_goal(goal) {
        return false;
    }
    // Durable economic work where the agent labors near coworkers. Movement-
    // only / non-work goals (Lead, Play, ProvideCare, FollowingPlayerCommand,
    // ReturnCamp, TameHorse, Defend, Raid, Scout, MigrateToCamp) are excluded.
    matches!(
        goal,
        AgentGoal::GatherFood
            | AgentGoal::GatherWood
            | AgentGoal::GatherStone
            | AgentGoal::Build
            | AgentGoal::Craft
            | AgentGoal::Farm
            | AgentGoal::Haul
            | AgentGoal::Stockpile
    )
}

/// Shared gate consumed by the four refactored consumer systems +
/// `social_fill_system` (NOT `tech_teaching_system`): dedicated
/// `AgentGoal::Socialize` (non-Dormant) **or** a live `SecondarySocial`.
#[inline]
pub fn is_social_contact(
    goal: AgentGoal,
    lod: LodLevel,
    secondary: Option<&SecondarySocial>,
    now: u32,
) -> bool {
    if lod == LodLevel::Dormant {
        return false;
    }
    if matches!(goal, AgentGoal::Socialize) {
        return true;
    }
    matches!(secondary, Some(s) if s.is_active(now))
}

/// Same-root-faction check, reusing the existing registry parent walk.
#[inline]
pub fn same_root_faction(reg: &FactionRegistry, a: u32, b: u32) -> bool {
    reg.root_faction(a) == reg.root_faction(b)
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

struct Snap {
    entity: Entity,
    tile: (i32, i32),
    root_faction: u32,
    compatible: bool,
    /// `Some(partner)` iff the agent currently has a *live* pairing.
    active_partner: Option<Entity>,
}

/// ParallelA, after `needs::tick_needs_system`, before
/// `goals::goal_update_system` / `opportunistic::opportunistic_interrupt_system`.
///
/// `SecondarySocial` is present on every Person from spawn; this system only
/// ever *mutates* it (stamp a live partner, or reset to inactive) — it never
/// inserts/removes the component. That is deliberate: insert/remove churns
/// the entity's archetype every rescan, which globally reorders `Query`
/// iteration, and the construction / seeding layout is sensitive to Person
/// iteration order. A uniform always-present component keeps every Person in
/// one stable archetype (== pre-feature baseline order).
///
/// Snapshot → field-mutate apply (two sequential borrows of the one query:
/// a read pass builds the snapshot, a write pass assigns) — no `Commands`,
/// no `&mut` aliasing. Pairing is **unilateral** (each agent independently
/// points at its own deterministic nearest valid coworker); the consumers
/// run their existing all-neighbor scan under the shared two-sided gate, so
/// strict mutual pairing is unnecessary.
#[allow(clippy::type_complexity)]
pub fn ambient_social_pairing_system(
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    mut q: Query<
        (
            Entity,
            &Transform,
            &AgentGoal,
            &LodLevel,
            &crate::simulation::faction::FactionMember,
            &mut SecondarySocial,
            (
                Option<&Drafted>,
                Option<&crate::simulation::combat::CombatTarget>,
                Option<&RescueTarget>,
                Option<&MigrationTarget>,
                Option<&PackingDuty>,
            ),
        ),
        With<Person>,
    >,
) {
    let now = clock.tick as u32;

    let mut snaps: Vec<Snap> = Vec::new();
    let mut index: ahash::AHashMap<Entity, usize> = ahash::AHashMap::default();
    for (e, t, goal, lod, fm, sec, (drafted, combat, rescue, migrate, packing)) in q.iter() {
        let tile = (
            (t.translation.x / TILE_SIZE).floor() as i32,
            (t.translation.y / TILE_SIZE).floor() as i32,
        );
        let in_combat = combat.map(|c| c.0.is_some()).unwrap_or(false);
        let migrating = migrate.is_some() || matches!(goal, AgentGoal::MigrateToCamp);
        let compatible = is_ambient_social_compatible(
            *goal,
            *lod,
            fm.faction_id,
            drafted.is_some(),
            in_combat,
            matches!(goal, AgentGoal::Raid),
            rescue.is_some(),
            migrating,
            packing.is_some(),
        );
        index.insert(e, snaps.len());
        snaps.push(Snap {
            entity: e,
            tile,
            root_faction: registry.root_faction(fm.faction_id),
            compatible,
            active_partner: if sec.is_active(now) {
                sec.partner
            } else {
                None
            },
        });
    }

    // Only entities whose `SecondarySocial` state actually changes are
    // recorded — the apply pass touches nobody else (zero needless writes,
    // never an archetype move).
    let mut decisions: ahash::AHashMap<Entity, SecondarySocial> = ahash::AHashMap::default();

    let partner_still_ok = |s: &Snap, partner: Entity| -> bool {
        index
            .get(&partner)
            .map(|&i| {
                let p = &snaps[i];
                p.compatible
                    && s.root_faction == p.root_faction
                    && chebyshev(s.tile, p.tile) <= SOCIAL_RADIUS
            })
            .unwrap_or(false)
    };

    for s in &snaps {
        let stagger = (s.entity.index() as u64) % PAIRING_RESCAN_CADENCE;
        let is_rescan = clock.tick % PAIRING_RESCAN_CADENCE == stagger && s.compatible;

        if is_rescan {
            // Deterministic nearest compatible same-root partner: explicit
            // min over (chebyshev, entity bits) — never rely on
            // `spatial.get` iteration order. Always (re)writes this agent's
            // state (fresh expiry, possible partner swap, or inactive).
            let mut best_key: Option<(i32, u64)> = None;
            let mut best_partner: Option<Entity> = None;
            for dy in -SOCIAL_RADIUS..=SOCIAL_RADIUS {
                for dx in -SOCIAL_RADIUS..=SOCIAL_RADIUS {
                    for &other in spatial.get(s.tile.0 + dx, s.tile.1 + dy) {
                        if other == s.entity {
                            continue;
                        }
                        let Some(&oi) = index.get(&other) else {
                            continue;
                        };
                        let o = &snaps[oi];
                        if !o.compatible || s.root_faction != o.root_faction {
                            continue;
                        }
                        let key = (chebyshev(s.tile, o.tile), other.to_bits());
                        if best_key.map(|b| key < b).unwrap_or(true) {
                            best_key = Some(key);
                            best_partner = Some(other);
                        }
                    }
                }
            }
            let new_val = match best_partner {
                Some(p) => SecondarySocial {
                    partner: Some(p),
                    mode: SocialMode::Ambient,
                    expires_tick: now + PAIRING_WINDOW,
                },
                None => SecondarySocial::inactive(),
            };
            // Skip only the no-op (still inactive). A same-partner rescan
            // must still rewrite to refresh `expires_tick`, otherwise a
            // long-stable pair lapses mid-`PAIRING_WINDOW`. Cost is one
            // field write per active agent per cadence (no archetype move).
            let still_inactive = best_partner.is_none() && s.active_partner.is_none();
            if !still_inactive {
                decisions.insert(s.entity, new_val);
            }
        } else if let Some(partner) = s.active_partner {
            // Off-cadence cleanup: drop a now-invalid live pairing
            // (partner despawned / left range / became incompatible, or
            // this agent became incompatible).
            if !s.compatible || !partner_still_ok(s, partner) {
                decisions.insert(s.entity, SecondarySocial::inactive());
            }
        }
    }

    if decisions.is_empty() {
        return;
    }
    for (e, _, _, _, _, mut sec, _) in q.iter_mut() {
        if let Some(v) = decisions.get(&e) {
            *sec = *v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compat(goal: AgentGoal) -> bool {
        is_ambient_social_compatible(
            goal,
            LodLevel::Full,
            1,
            false,
            false,
            false,
            false,
            false,
            false,
        )
    }

    #[test]
    fn accepts_worker_goals() {
        assert!(compat(AgentGoal::GatherWood));
        assert!(compat(AgentGoal::Build));
        assert!(compat(AgentGoal::Farm));
        assert!(compat(AgentGoal::Haul));
        assert!(compat(AgentGoal::Stockpile));
    }

    #[test]
    fn rejects_solo() {
        assert!(!is_ambient_social_compatible(
            AgentGoal::GatherWood,
            LodLevel::Full,
            SOLO,
            false,
            false,
            false,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn rejects_dormant() {
        assert!(!is_ambient_social_compatible(
            AgentGoal::GatherWood,
            LodLevel::Dormant,
            1,
            false,
            false,
            false,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn rejects_dedicated_socialize() {
        assert!(!compat(AgentGoal::Socialize));
    }

    #[test]
    fn rejects_maintenance_goals() {
        for g in [
            AgentGoal::Survive,
            AgentGoal::Drink,
            AgentGoal::Sleep,
            AgentGoal::SeekCare,
        ] {
            assert!(!compat(g), "{g:?} should be rejected");
        }
    }

    #[test]
    fn rejects_non_work_goals() {
        for g in [
            AgentGoal::Lead,
            AgentGoal::Play,
            AgentGoal::ProvideCare,
            AgentGoal::Defend,
            AgentGoal::Raid,
            AgentGoal::FollowingPlayerCommand,
            AgentGoal::Scout,
            AgentGoal::MigrateToCamp,
        ] {
            assert!(!compat(g), "{g:?} should be rejected");
        }
    }

    #[test]
    fn rejects_each_incompatible_flag() {
        let base = |drafted, combat, raid, rescue, migrate, packing| {
            is_ambient_social_compatible(
                AgentGoal::GatherWood,
                LodLevel::Full,
                1,
                drafted,
                combat,
                raid,
                rescue,
                migrate,
                packing,
            )
        };
        assert!(base(false, false, false, false, false, false));
        assert!(!base(true, false, false, false, false, false));
        assert!(!base(false, true, false, false, false, false));
        assert!(!base(false, false, true, false, false, false));
        assert!(!base(false, false, false, true, false, false));
        assert!(!base(false, false, false, false, true, false));
        assert!(!base(false, false, false, false, false, true));
    }

    #[test]
    fn social_contact_socialize_always_true_non_dormant() {
        assert!(is_social_contact(
            AgentGoal::Socialize,
            LodLevel::Full,
            None,
            0
        ));
        assert!(!is_social_contact(
            AgentGoal::Socialize,
            LodLevel::Dormant,
            None,
            0
        ));
    }

    #[test]
    fn social_contact_secondary_strict_expiry() {
        let s = SecondarySocial {
            partner: Some(Entity::from_raw(7)),
            mode: SocialMode::Ambient,
            expires_tick: 100,
        };
        assert!(is_social_contact(
            AgentGoal::GatherWood,
            LodLevel::Full,
            Some(&s),
            50
        ));
        // strict: expires_tick == now is expired
        assert!(!is_social_contact(
            AgentGoal::GatherWood,
            LodLevel::Full,
            Some(&s),
            100
        ));
        // inactive resting state is never a contact
        let inactive = SecondarySocial::inactive();
        assert!(!is_social_contact(
            AgentGoal::GatherWood,
            LodLevel::Full,
            Some(&inactive),
            0
        ));
        assert!(!is_social_contact(
            AgentGoal::GatherWood,
            LodLevel::Full,
            None,
            0
        ));
    }

    #[test]
    fn inactive_is_not_active_active_is() {
        let inactive = SecondarySocial::inactive();
        assert!(!inactive.is_active(0));
        assert!(!inactive.is_active(999));
        let live = SecondarySocial {
            partner: Some(Entity::from_raw(3)),
            mode: SocialMode::Ambient,
            expires_tick: 50,
        };
        assert!(live.is_active(49));
        assert!(!live.is_active(50)); // strict
                                      // partner None ⇒ never active even with future expiry
        let no_partner = SecondarySocial {
            partner: None,
            mode: SocialMode::Ambient,
            expires_tick: 9999,
        };
        assert!(!no_partner.is_active(0));
    }
}
