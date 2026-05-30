//! Phase 2 — Capital recognition (wage-aware-labor-market-v2).
//!
//! Helpers that score how well-equipped an agent is to practise a given
//! profession. Three orthogonal axes:
//!
//! - **Tool affinity** — does the agent (or their hands) carry an item
//!   that maps to the profession via catalog tags / known IDs?
//! - **Workshop affinity** — is there a profession-appropriate building
//!   owned by the agent's village faction (or, better, their household
//!   sub-faction) within a small radius?
//! - **Land affinity** — does the agent's household hold an Agricultural
//!   plot (Farmer only)?
//!
//! All three are read-only over existing state. The `OwnedBy` component +
//! `WorkshopOwnership` resource introduced here are the only new storage
//! Phase 2 adds; Phase 4 (EV-driven profession choice) will consume them.
//!
//! The plan calls out a richer tool catalog (Hoe / Awl / Loom shuttle /
//! Hammer / Bow) gated on tech. That catalog split is deferred — the
//! current `tool_profession` lookup keys off the existing `weapon` /
//! `tools` resources via `core_ids` so the helper is forward-compatible
//! once those entries land.
//!
//! `Profession::Crafter` and `Profession::Healer` are introduced in
//! Phase 5; `tool_profession` already maps `tools` → `Crafter` as a
//! sentinel that returns `None` today (because the variant doesn't
//! exist), and will Just Work when the variant lands.

use crate::collections::AHashMap;
use bevy::ecs::component::ComponentId;
use bevy::ecs::world::DeferredWorld;
use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::simulation::carry::Carrier;
use crate::simulation::land::{Plot, PlotIndex, TenureHolder};
use crate::simulation::person::Profession;
use crate::simulation::reproduction::HouseholdMember;
use crate::simulation::settlement::ZoneKind;

/// Workshop kinds for which Phase 2 wires profession affinity. Mirrors
/// the structure spawn sites in `construction.rs`; one variant per
/// `BuildSiteKind` that produces a workshop-class building.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkshopKind {
    Workbench,
    Loom,
    Market,
    Granary,
    Shrine,
    Barracks,
    Monument,
}

impl WorkshopKind {
    /// `true` when this workshop boosts capital for the given profession.
    /// `Profession::None` always returns `false`.
    pub fn affine_to(self, prof: Profession) -> bool {
        match prof {
            Profession::Bureaucrat => matches!(self, WorkshopKind::Market),
            // Phase 5a: Crafter — Workbench is the primary station for
            // tool/weapon recipes; Loom for cloth recipes. Both lift
            // EV when within `WORKSHOP_AFFINITY_RADIUS` of the agent.
            Profession::Crafter => matches!(self, WorkshopKind::Workbench | WorkshopKind::Loom),
            // Phase 5b-stretch: Healer is shrine-affine. Scaffolding-
            // only today — no auto-promotion path until a Heal-job
            // pipeline lands — but the workshop term is correct so the
            // inspector's EV table and the cross-switcher's `EV(Healer)`
            // computation read a non-trivial capital factor when a
            // household holds a Shrine.
            Profession::Healer => matches!(self, WorkshopKind::Shrine),
            _ => false,
        }
    }
}

/// Stamped at workshop finalize. Carries everything the
/// `WorkshopOwnership` add/remove hooks need without re-reading the
/// individual `Workbench` / `Market` / `Loom` components.
#[derive(Component, Clone, Copy, Debug)]
pub struct OwnedBy {
    pub faction_id: u32,
    pub kind: WorkshopKind,
    pub tile: (i32, i32),
}

#[derive(Clone, Copy, Debug)]
pub struct WorkshopEntry {
    pub entity: Entity,
    pub kind: WorkshopKind,
    pub tile: (i32, i32),
}

/// Faction → list of owned workshops. Maintained by the `OwnedBy` add /
/// remove hooks (`on_owned_by_add` / `on_owned_by_remove`).
#[derive(Resource, Default)]
pub struct WorkshopOwnership {
    pub by_faction: AHashMap<u32, Vec<WorkshopEntry>>,
}

impl WorkshopOwnership {
    pub fn workshops_for(&self, faction_id: u32) -> &[WorkshopEntry] {
        self.by_faction
            .get(&faction_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

pub fn on_owned_by_add(mut world: DeferredWorld<'_>, entity: Entity, _: ComponentId) {
    let Some(owner) = world.get::<OwnedBy>(entity).copied() else {
        return;
    };
    let mut ownership = world.resource_mut::<WorkshopOwnership>();
    let entries = ownership.by_faction.entry(owner.faction_id).or_default();
    if !entries.iter().any(|e| e.entity == entity) {
        entries.push(WorkshopEntry {
            entity,
            kind: owner.kind,
            tile: owner.tile,
        });
    }
}

pub fn on_owned_by_remove(mut world: DeferredWorld<'_>, entity: Entity, _: ComponentId) {
    let Some(owner) = world.get::<OwnedBy>(entity).copied() else {
        return;
    };
    let mut ownership = world.resource_mut::<WorkshopOwnership>();
    if let Some(entries) = ownership.by_faction.get_mut(&owner.faction_id) {
        entries.retain(|e| e.entity != entity);
    }
}

// ─── Tool affinity ──────────────────────────────────────────────────────

/// Map a tool/weapon `ResourceId` to the profession it boosts, if any.
///
/// Today this matches by canonical core ids — `weapon` → `Hunter`. The
/// Phase 2a catalog split (Hoe / Awl / Loom shuttle / Hammer / Bow with
/// explicit `prof:*` tags) will replace this with a tag walk; signature
/// stays stable.
pub fn tool_profession(rid: ResourceId) -> Option<Profession> {
    // Cache the core ids — they never change after `init_core_ids`.
    if rid == core_ids::weapon() {
        return Some(Profession::Hunter);
    }
    if rid == core_ids::tools() {
        return Some(Profession::Crafter);
    }
    None
}

/// Tool capital factor: `1.0` baseline; `1.5` when any tool in the
/// agent's inventory or hands maps to `profession`. Capital factors all
/// share this 1.0-baseline / 1.5-boosted shape so they compose cleanly
/// when averaged in Phase 4's EV computation.
pub fn tool_capital_factor(
    agent: &EconomicAgent,
    carrier: &Carrier,
    profession: Profession,
) -> f32 {
    if profession == Profession::None {
        return 1.0;
    }
    let has_in_inv = agent
        .iter_resource_stacks()
        .any(|(rid, qty)| qty > 0 && tool_profession(rid) == Some(profession));
    if has_in_inv {
        return 1.5;
    }
    // `Carrier::quantity_of_resource` covers both hand slots. Probe
    // each canonical tool resource that maps to `profession`; today
    // that's a single ID per profession (weapon → Hunter, tools →
    // Crafter), but the catalog-tag split will fold more here.
    let hand_probe = match profession {
        Profession::Hunter => Some(core_ids::weapon()),
        Profession::Crafter => Some(core_ids::tools()),
        _ => None,
    };
    if let Some(rid) = hand_probe {
        if carrier.quantity_of_resource(rid) > 0 {
            return 1.5;
        }
    }
    1.0
}

// ─── Workshop affinity ──────────────────────────────────────────────────

/// Chebyshev radius within which a workshop counts as "the agent's
/// workshop" for capital scoring. Matches the chief-posting workbench
/// proximity gate (`<= 12` from `home_tile`).
pub const WORKSHOP_AFFINITY_RADIUS: i32 = 12;

/// Workshop capital factor: `1.0` baseline; `+0.5` for a profession-
/// affine workshop owned by the agent's village faction within radius;
/// `+1.0` instead when the workshop is owned by the agent's household
/// sub-faction (household ownership dominates village ownership).
pub fn workshop_capital_factor(
    agent_tile: (i32, i32),
    village_faction_id: u32,
    household_id: Option<u32>,
    profession: Profession,
    ownership: &WorkshopOwnership,
) -> f32 {
    if profession == Profession::None {
        return 1.0;
    }
    let mut best: f32 = 0.0;
    if let Some(hid) = household_id {
        if any_affine_within(ownership.workshops_for(hid), agent_tile, profession) {
            best = best.max(1.0);
        }
    }
    if any_affine_within(
        ownership.workshops_for(village_faction_id),
        agent_tile,
        profession,
    ) {
        best = best.max(0.5);
    }
    1.0 + best
}

fn any_affine_within(
    workshops: &[WorkshopEntry],
    from: (i32, i32),
    profession: Profession,
) -> bool {
    workshops.iter().any(|w| {
        w.kind.affine_to(profession) && chebyshev(w.tile, from) <= WORKSHOP_AFFINITY_RADIUS
    })
}

fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

// ─── Land affinity ──────────────────────────────────────────────────────

/// Land capital factor (Farmer only): `1.0` baseline; `1.5` when the
/// agent's household holds an Agricultural plot under any non-StateOwned
/// tenure (Leased / Sharecropping / Freehold). Non-Farmer professions
/// always return `1.0` — land doesn't multiply a Hunter's productivity.
///
/// The check walks `PlotIndex.by_settlement` for plots whose `holder`
/// resolves to `Household { faction_id: household_id }` — sharecrop and
/// freehold both qualify. State-owned plots return `1.0` (the household
/// has no durable claim on the parcel).
pub fn land_capital_factor(
    household: Option<&HouseholdMember>,
    profession: Profession,
    plot_q: &Query<&Plot>,
    plot_index: &PlotIndex,
) -> f32 {
    if profession != Profession::Farmer {
        return 1.0;
    }
    let Some(hm) = household else {
        return 1.0;
    };
    let hid = hm.household_id;
    // Walk every plot we know about. `PlotIndex.by_settlement` keys by
    // village faction_id; we don't know which village so walk by_id.
    for (_pid, entity) in plot_index.by_id.iter() {
        let Ok(plot) = plot_q.get(*entity) else {
            continue;
        };
        if plot.zone_kind != ZoneKind::Agricultural {
            continue;
        }
        if let TenureHolder::Household { faction_id } = plot.holder {
            if faction_id == hid {
                return 1.5;
            }
        }
    }
    1.0
}

// ─── Composite ──────────────────────────────────────────────────────────

/// Phase 4's EV computation averages the three capital factors; expose
/// the formula here so the EV code can call one function. The average is
/// in `[1.0, 1.5]` (or up to `~1.83` when both household workshop and a
/// matching tool/land bonus stack — bounded by construction since each
/// term is in `[1.0, 2.0]`).
pub fn capital_factor(
    agent: &EconomicAgent,
    carrier: &Carrier,
    agent_tile: (i32, i32),
    village_faction_id: u32,
    household: Option<&HouseholdMember>,
    profession: Profession,
    ownership: &WorkshopOwnership,
    plot_q: &Query<&Plot>,
    plot_index: &PlotIndex,
) -> f32 {
    let t = tool_capital_factor(agent, carrier, profession);
    let w = workshop_capital_factor(
        agent_tile,
        village_faction_id,
        household.map(|h| h.household_id),
        profession,
        ownership,
    );
    let l = land_capital_factor(household, profession, plot_q, plot_index);
    (t + w + l) / 3.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workshop_kind_affine_to() {
        assert!(WorkshopKind::Market.affine_to(Profession::Bureaucrat));
        assert!(!WorkshopKind::Workbench.affine_to(Profession::Bureaucrat));
        assert!(!WorkshopKind::Market.affine_to(Profession::None));
        assert!(!WorkshopKind::Market.affine_to(Profession::Farmer));
    }

    #[test]
    fn workshop_kind_affine_to_crafter() {
        // Phase 5a: Workbench and Loom both lift Crafter EV.
        assert!(WorkshopKind::Workbench.affine_to(Profession::Crafter));
        assert!(WorkshopKind::Loom.affine_to(Profession::Crafter));
        // Market is Bureaucrat-affine, not Crafter.
        assert!(!WorkshopKind::Market.affine_to(Profession::Crafter));
        // Crafter doesn't pick up Granary / Shrine / Barracks / Monument.
        assert!(!WorkshopKind::Granary.affine_to(Profession::Crafter));
        assert!(!WorkshopKind::Shrine.affine_to(Profession::Crafter));
    }

    #[test]
    fn chebyshev_distance() {
        assert_eq!(chebyshev((0, 0), (3, 4)), 4);
        assert_eq!(chebyshev((-2, -2), (1, 0)), 3);
        assert_eq!(chebyshev((5, 5), (5, 5)), 0);
    }
}
