//! Per-faction settlement plans: zoned regions around the home tile that
//! direct where buildings of a given kind may be placed.
//!
//! The planner system that populates these is added in Phase 2; this module
//! defines the data model so other systems (debug panel, build selector) can
//! reference the types now.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::simulation::faction::{FactionData, FactionRegistry, LayoutStyle, PlayerFaction, SOLO};
use crate::simulation::schedule::SimClock;
use crate::simulation::technology::{
    CROP_CULTIVATION, FLINT_KNAPPING, LONG_DIST_TRADE, PERM_SETTLEMENT, SACRED_RITUAL,
};
use crate::world::terrain::TILE_SIZE;

/// Inclusive-exclusive rectangle in tile coordinates: tiles `(x, y)` with
/// `x0 <= x < x0+w`, `y0 <= y < y0+h`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRect {
    pub x0: i16,
    pub y0: i16,
    pub w: u16,
    pub h: u16,
}

impl TileRect {
    pub fn new(x0: i16, y0: i16, w: u16, h: u16) -> Self {
        Self { x0, y0, w, h }
    }

    #[inline]
    pub fn contains(&self, x: i16, y: i16) -> bool {
        x >= self.x0
            && y >= self.y0
            && (x as i32) < self.x0 as i32 + self.w as i32
            && (y as i32) < self.y0 as i32 + self.h as i32
    }

    pub fn center(&self) -> (i16, i16) {
        (
            (self.x0 as i32 + self.w as i32 / 2) as i16,
            (self.y0 as i32 + self.h as i32 / 2) as i16,
        )
    }

    pub fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }
}

/// Functional category of a zone within a settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneKind {
    /// Beds, longhouses, dwellings.
    Residential,
    /// Farmland, planted fields.
    Agricultural,
    /// Workbenches, looms, smithies.
    Crafting,
    /// Civic buildings: granary, scribe house, table+chair commons.
    Civic,
    /// Walls, gate, barracks.
    Defense,
    /// Storage tiles, warehouses.
    Storage,
    /// Shrines, monuments.
    Sacred,
    /// Markets, traders.
    Market,
}

impl ZoneKind {
    pub fn label(self) -> &'static str {
        match self {
            ZoneKind::Residential => "Residential",
            ZoneKind::Agricultural => "Agricultural",
            ZoneKind::Crafting => "Crafting",
            ZoneKind::Civic => "Civic",
            ZoneKind::Defense => "Defense",
            ZoneKind::Storage => "Storage",
            ZoneKind::Sacred => "Sacred",
            ZoneKind::Market => "Market",
        }
    }
}

/// A single zoned region within a settlement.
#[derive(Clone, Debug)]
pub struct Zone {
    pub kind: ZoneKind,
    pub rect: TileRect,
    /// 0..=255 — planner's intrinsic priority for placing the next building here.
    pub priority: u8,
    /// Target number of buildings the planner expects this zone to hold.
    pub capacity: u8,
    /// How many buildings have actually been placed in this zone (saturating).
    pub filled: u8,
}

/// Cardinal/diagonal axis along which the planner has carved a road spine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    NorthSouth,
    EastWest,
    NeSw,
    NwSe,
}

/// Per-faction settlement plan. Re-evaluated periodically by the planner; the
/// build selector consumes it to decide where to place new blueprints.
#[derive(Clone, Debug, Default)]
pub struct SettlementPlan {
    pub zones: Vec<Zone>,
    pub road_spine: Vec<Axis>,
    /// Tick at which the plan was last (re)computed. 0 = never planned.
    pub planned_at_tick: u64,
    /// Hash of inputs that, if changed, force a re-plan
    /// (`techs.0 + member_count_bucket + culture.style`).
    pub culture_hash: u64,
}

impl SettlementPlan {
    pub fn zone_for(&self, kind: ZoneKind, x: i16, y: i16) -> Option<&Zone> {
        self.zones
            .iter()
            .find(|z| z.kind == kind && z.rect.contains(x, y))
    }
}

/// Per-faction settlement plans, keyed by faction_id.
#[derive(Resource, Default)]
pub struct SettlementPlans(pub AHashMap<u32, SettlementPlan>);

// ── Planner ──────────────────────────────────────────────────────────────────

/// Tick interval after which a plan is considered stale and may be rebuilt
/// even if no input changed.
const REPLAN_INTERVAL: u64 = 600;

/// Compute the input hash that, when changed, forces a re-plan.
fn culture_hash(faction: &FactionData) -> u64 {
    let pop_bucket = (faction.member_count / 5) as u64;
    faction.techs.0
        ^ (pop_bucket << 56)
        ^ ((faction.culture.style as u64) << 48)
        ^ ((faction.culture.density as u64) << 40)
        ^ ((faction.culture.defensive as u64) << 32)
}

/// Convenience: build a zone with the given footprint.
#[inline]
fn zone(kind: ZoneKind, x0: i32, y0: i32, w: u32, h: u32, priority: u8, capacity: u8) -> Zone {
    let w = w.max(1).min(u16::MAX as u32) as u16;
    let h = h.max(1).min(u16::MAX as u32) as u16;
    let x0 = x0.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let y0 = y0.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    Zone {
        kind,
        rect: TileRect::new(x0, y0, w, h),
        priority,
        capacity,
        filled: 0,
    }
}

/// Hearth offset radii used by the Paleolithic band-camp planner. Exposed so
/// the build selector can mirror them when picking campfire targets if the
/// settlement plan is briefly absent.
pub const PALEO_PRIMARY_HEARTH_DIST: f32 = 5.0;
pub const PALEO_SECONDARY_HEARTH_DIST: f32 = 12.0;

/// Number of hearths a Paleolithic band camp targets given a member count.
/// One hearth per ~6 members. The actual hearth-queue cadence is gated by
/// crescent saturation + bed deficit in `generate_candidates`, so this is an
/// upper bound, not a hard cap.
pub fn paleolithic_hearth_count(members: u32) -> u32 {
    ((members + 5) / 6).max(1)
}

/// Compute the deterministic Paleolithic hearth positions for a faction.
/// `faction_id` selects the primary angle so different bands face different
/// directions; subsequent hearths fan around the home at secondary distance,
/// pushing outward each lap so unbounded member counts still yield distinct
/// positions.
pub fn paleolithic_hearth_positions(
    faction_id: u32,
    home: (i16, i16),
    members: u32,
) -> Vec<(i32, i32)> {
    let n = paleolithic_hearth_count(members);
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    let theta0 =
        (faction_id.wrapping_mul(2654435761) as f32 / u32::MAX as f32) * std::f32::consts::TAU;
    let mut hearths = Vec::with_capacity(n as usize);
    hearths.push((
        (hx as f32 + theta0.cos() * PALEO_PRIMARY_HEARTH_DIST).round() as i32,
        (hy as f32 + theta0.sin() * PALEO_PRIMARY_HEARTH_DIST).round() as i32,
    ));
    // Preserved n=2,3 angles (±π/2±0.3) lead, then fill the remaining ring.
    let offsets: [f32; 8] = [
        std::f32::consts::FRAC_PI_2 + 0.3,
        -std::f32::consts::FRAC_PI_2 - 0.3,
        std::f32::consts::PI,
        2.0 * std::f32::consts::PI / 3.0,
        -2.0 * std::f32::consts::PI / 3.0,
        std::f32::consts::FRAC_PI_4,
        -std::f32::consts::FRAC_PI_4,
        std::f32::consts::PI - std::f32::consts::FRAC_PI_4,
    ];
    for k in 1..n as usize {
        let off = offsets[(k - 1) % offsets.len()];
        let lap = ((k - 1) / offsets.len()) as f32;
        let scale = 1.0 + 0.18 * lap;
        let theta = theta0 + off;
        hearths.push((
            (hx as f32 + theta.cos() * PALEO_SECONDARY_HEARTH_DIST * scale).round() as i32,
            (hy as f32 + theta.sin() * PALEO_SECONDARY_HEARTH_DIST * scale).round() as i32,
        ));
    }
    hearths
}

/// Build a fresh `SettlementPlan` for a faction, choosing zone shapes from
/// the faction's `LayoutStyle` and tech progression. Procedural — no terrain
/// scoring yet (Phase 3+).
pub fn build_settlement_plan(faction_id: u32, faction: &FactionData, tick: u64) -> SettlementPlan {
    let (hx, hy) = (faction.home_tile.0 as i32, faction.home_tile.1 as i32);
    let style = faction.culture.style;
    let members = faction.member_count.max(2);
    let techs = &faction.techs;

    if !techs.has(PERM_SETTLEMENT) {
        return build_paleolithic_plan(faction_id, faction, tick);
    }

    // Base radius scales sub-linearly with population.
    let base_r: i32 = (4 + (members as f32).sqrt() as i32 * 2).clamp(6, 24);

    // Density (255) ⇒ tight zones; density (0) ⇒ loose zones.
    let gap: i32 = ((255 - faction.culture.density as i32) / 50).max(0); // 0..=5
                                                                         // Defensive trait grows the wall ring outward.
    let def_pad: i32 = (faction.culture.defensive as i32 / 80).max(1); // 1..=3

    let mut zones: Vec<Zone> = Vec::new();
    let mut road_spine: Vec<Axis> = Vec::new();

    match style {
        LayoutStyle::Radial => {
            // Civic core, four residential quadrants in a ring, defense wraps everything.
            zones.push(zone(ZoneKind::Civic, hx - 2, hy - 2, 5, 5, 110, 1));
            let r = base_r;
            let arm_w = (r as u32).max(5);
            zones.push(zone(
                ZoneKind::Residential,
                hx - r - gap,
                hy - 3,
                arm_w,
                7,
                200,
                (members.min(20) / 4 + 2) as u8,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx + 3 + gap,
                hy - 3,
                arm_w,
                7,
                200,
                (members.min(20) / 4 + 2) as u8,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx - 3,
                hy - r - gap,
                7,
                arm_w,
                200,
                (members.min(20) / 4 + 2) as u8,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx - 3,
                hy + 3 + gap,
                7,
                arm_w,
                200,
                (members.min(20) / 4 + 2) as u8,
            ));
            let dr = r + def_pad + gap;
            zones.push(zone(
                ZoneKind::Defense,
                hx - dr,
                hy - dr,
                (dr as u32 * 2) as u32,
                (dr as u32 * 2) as u32,
                180,
                12,
            ));
            zones.push(zone(ZoneKind::Storage, hx + 1, hy - 1, 3, 3, 150, 2));
            road_spine.push(Axis::NorthSouth);
            road_spine.push(Axis::EastWest);
        }
        LayoutStyle::Linear => {
            // Long E-W spine; residential arms east and west of home.
            zones.push(zone(ZoneKind::Civic, hx - 2, hy - 2, 5, 5, 110, 1));
            let arm = (base_r * 3 / 2) as u32;
            zones.push(zone(
                ZoneKind::Residential,
                hx - arm as i32 - gap,
                hy - 2,
                arm,
                5,
                200,
                (members / 2 + 1).min(10) as u8,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx + 3 + gap,
                hy - 2,
                arm,
                5,
                200,
                (members / 2 + 1).min(10) as u8,
            ));
            let outer = arm as i32 + def_pad;
            zones.push(zone(
                ZoneKind::Defense,
                hx - outer,
                hy - 4,
                (outer as u32 * 2) + 5,
                9,
                160,
                10,
            ));
            zones.push(zone(ZoneKind::Storage, hx - 1, hy - 4, 3, 3, 150, 2));
            road_spine.push(Axis::EastWest);
        }
        LayoutStyle::Compact => {
            // Tight square footprint; everything packed.
            let r = (base_r * 3 / 4).max(5);
            zones.push(zone(ZoneKind::Civic, hx - 1, hy - 1, 3, 3, 110, 1));
            zones.push(zone(
                ZoneKind::Residential,
                hx - r,
                hy - r,
                (r as u32 * 2),
                (r as u32 * 2),
                200,
                members.min(20) as u8,
            ));
            let dr = r + def_pad;
            zones.push(zone(
                ZoneKind::Defense,
                hx - dr,
                hy - dr,
                (dr as u32 * 2),
                (dr as u32 * 2),
                180,
                10,
            ));
            zones.push(zone(ZoneKind::Storage, hx, hy, 2, 2, 150, 2));
            road_spine.push(Axis::EastWest);
        }
        LayoutStyle::Sprawling => {
            // Large radius, wide gaps, four splayed-out arms.
            let r = base_r * 2;
            let g = (gap.max(2)) as i32;
            zones.push(zone(ZoneKind::Civic, hx - 2, hy - 2, 5, 5, 110, 1));
            let arm = r as u32;
            zones.push(zone(
                ZoneKind::Residential,
                hx - arm as i32 - g,
                hy - 3 - g,
                arm,
                7,
                200,
                5,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx + 3 + g,
                hy - 3 - g,
                arm,
                7,
                200,
                5,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx - 3 - g,
                hy - arm as i32 - g,
                7,
                arm,
                200,
                5,
            ));
            zones.push(zone(
                ZoneKind::Residential,
                hx - 3 - g,
                hy + 3 + g,
                7,
                arm,
                200,
                5,
            ));
            let dr = r + def_pad + g;
            zones.push(zone(
                ZoneKind::Defense,
                hx - dr,
                hy - dr,
                (dr as u32 * 2),
                (dr as u32 * 2),
                140,
                14,
            ));
            zones.push(zone(ZoneKind::Storage, hx - 3, hy + 3, 3, 3, 150, 2));
            road_spine.push(Axis::NorthSouth);
            road_spine.push(Axis::EastWest);
            road_spine.push(Axis::NeSw);
            road_spine.push(Axis::NwSe);
        }
        LayoutStyle::Citadel => {
            // Inner residential walled tightly; agriculture in a wide outer ring.
            let inner = (base_r * 2 / 3).max(5);
            let outer = base_r * 2;
            zones.push(zone(ZoneKind::Civic, hx - 1, hy - 1, 3, 3, 110, 1));
            zones.push(zone(
                ZoneKind::Residential,
                hx - inner,
                hy - inner,
                (inner as u32 * 2),
                (inner as u32 * 2),
                200,
                6,
            ));
            let dr = inner + def_pad;
            zones.push(zone(
                ZoneKind::Defense,
                hx - dr,
                hy - dr,
                (dr as u32 * 2),
                (dr as u32 * 2),
                240,
                16,
            ));
            zones.push(zone(
                ZoneKind::Agricultural,
                hx - outer,
                hy - outer,
                (outer as u32 * 2),
                (outer as u32 * 2),
                100,
                16,
            ));
            zones.push(zone(
                ZoneKind::Storage,
                hx + inner + 1,
                hy - 1,
                3,
                3,
                150,
                2,
            ));
            road_spine.push(Axis::NorthSouth);
            road_spine.push(Axis::EastWest);
        }
    }

    // ── Tech-gated optional zones ────────────────────────────────────────────
    if techs.has(CROP_CULTIVATION) && !zones.iter().any(|z| z.kind == ZoneKind::Agricultural) {
        // Place ag zone outside defense ring on the south side.
        let r = base_r * 2 + def_pad + 2;
        zones.push(zone(
            ZoneKind::Agricultural,
            hx - r,
            hy + r,
            (r as u32) * 2,
            8,
            100,
            12,
        ));
    }
    if techs.has(FLINT_KNAPPING) {
        // Crafting zone — west edge of residential cluster.
        zones.push(zone(ZoneKind::Crafting, hx - base_r, hy + 1, 5, 4, 130, 2));
    }
    if techs.has(SACRED_RITUAL) {
        // Sacred zone — bias toward center for ceremonial cultures.
        let cer = faction.culture.ceremonial as i32;
        let dx = if cer > 180 { 0 } else { 4 };
        zones.push(zone(
            ZoneKind::Sacred,
            hx - 2 - dx,
            hy + 1,
            5,
            4,
            120 + (cer / 4) as u8,
            1,
        ));
    }
    if techs.has(LONG_DIST_TRADE) {
        // Market — adjacent to storage.
        let mer = faction.culture.mercantile as i32;
        let cap = if mer > 180 { 2 } else { 1 };
        zones.push(zone(
            ZoneKind::Market,
            hx + 4,
            hy + 1,
            5,
            4,
            100 + (mer / 4) as u8,
            cap,
        ));
    }

    SettlementPlan {
        zones,
        road_spine,
        planned_at_tick: tick,
        culture_hash: culture_hash(faction),
    }
}

/// Hearth-and-cluster layout for pre-settlement bands: a small Civic anchor
/// for each hearth plus a per-hearth Residential bbox that the build selector
/// uses to cluster sleeping spots around the fire.
fn build_paleolithic_plan(faction_id: u32, faction: &FactionData, tick: u64) -> SettlementPlan {
    let (hx, hy) = (faction.home_tile.0 as i32, faction.home_tile.1 as i32);
    let members = faction.member_count.max(1);
    let techs = &faction.techs;

    let hearths = paleolithic_hearth_positions(faction_id, faction.home_tile, members);
    let n_hearths = hearths.len() as u32;
    let beds_per_hearth = (((members + n_hearths - 1) / n_hearths) as i32).clamp(2, 6) as u8;

    let mut zones: Vec<Zone> = Vec::with_capacity(hearths.len() * 2 + 2);

    for &(hxh, hyh) in &hearths {
        // Civic anchor — campfire site. Small so the hearth sits at the
        // offset, not adjacent to the faction-center tile.
        zones.push(zone(ZoneKind::Civic, hxh - 1, hyh - 1, 3, 3, 200, 1));
        // Residential bbox — the build selector filters tiles in an annulus
        // around the hearth itself, so this rect just delimits the search
        // area for the overlay and other zone-aware code paths.
        zones.push(zone(
            ZoneKind::Residential,
            hxh - 6,
            hyh - 6,
            13,
            13,
            180,
            beds_per_hearth,
        ));
    }

    if techs.has(FLINT_KNAPPING) {
        // Anchor crafting near the primary hearth so toolmakers stay by the fire.
        let (hxh, hyh) = hearths[0];
        zones.push(zone(ZoneKind::Crafting, hxh - 2, hyh + 3, 5, 4, 130, 2));
    }
    if techs.has(SACRED_RITUAL) {
        let cer = faction.culture.ceremonial as i32;
        let dx = if cer > 180 { 0 } else { 4 };
        zones.push(zone(
            ZoneKind::Sacred,
            hx - 2 - dx,
            hy + 1,
            5,
            4,
            120 + (cer / 4) as u8,
            1,
        ));
    }

    SettlementPlan {
        zones,
        road_spine: Vec::new(),
        planned_at_tick: tick,
        culture_hash: culture_hash(faction),
    }
}

/// Toggles the gizmo overlay that draws zone rectangles for the player's
/// faction over the world. Lives next to the planner so the rendering stays
/// data-adjacent.
#[derive(Resource)]
pub struct ZoneOverlayToggle {
    pub show: bool,
    pub all_factions: bool,
}

impl Default for ZoneOverlayToggle {
    fn default() -> Self {
        Self {
            show: false,
            all_factions: false,
        }
    }
}

fn zone_color(kind: ZoneKind) -> Color {
    match kind {
        ZoneKind::Residential => Color::srgba(0.95, 0.75, 0.30, 0.85),
        ZoneKind::Agricultural => Color::srgba(0.45, 0.85, 0.30, 0.85),
        ZoneKind::Crafting => Color::srgba(0.85, 0.55, 0.20, 0.85),
        ZoneKind::Civic => Color::srgba(0.95, 0.95, 0.30, 0.85),
        ZoneKind::Defense => Color::srgba(0.85, 0.30, 0.30, 0.85),
        ZoneKind::Storage => Color::srgba(0.50, 0.70, 0.95, 0.85),
        ZoneKind::Sacred => Color::srgba(0.85, 0.45, 0.95, 0.85),
        ZoneKind::Market => Color::srgba(0.30, 0.85, 0.85, 0.85),
    }
}

/// Render zone outlines as gizmos for visual debugging.
pub fn zone_overlay_gizmo_system(
    mut gizmos: Gizmos,
    plans: Res<SettlementPlans>,
    player_faction: Res<PlayerFaction>,
    toggle: Res<ZoneOverlayToggle>,
) {
    if !toggle.show {
        return;
    }
    let plans_iter: Vec<(&u32, &SettlementPlan)> = if toggle.all_factions {
        plans.0.iter().collect()
    } else {
        plans
            .0
            .get(&player_faction.faction_id)
            .map(|p| vec![(&player_faction.faction_id, p)])
            .unwrap_or_default()
    };

    for (_fid, plan) in plans_iter {
        for zone in &plan.zones {
            let r = zone.rect;
            let x_min = r.x0 as f32 * TILE_SIZE;
            let y_min = r.y0 as f32 * TILE_SIZE;
            let x_max = (r.x0 as f32 + r.w as f32) * TILE_SIZE;
            let y_max = (r.y0 as f32 + r.h as f32) * TILE_SIZE;
            let cx = (x_min + x_max) * 0.5;
            let cy = (y_min + y_max) * 0.5;
            let size = Vec2::new(x_max - x_min, y_max - y_min);
            gizmos.rect_2d(
                Isometry2d::from_translation(Vec2::new(cx, cy)),
                size,
                zone_color(zone.kind),
            );
        }
    }
}

/// System: re-evaluates each non-SOLO faction's settlement plan periodically.
/// Throttled — at most one faction is re-planned per tick to spread CPU cost.
pub fn settlement_planner_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    mut plans: ResMut<SettlementPlans>,
) {
    let tick = clock.tick;

    // Cheap stagger: only consider factions whose id matches the tick mod 60.
    // Combined with the staleness check below, every faction is reconsidered
    // within ~60 ticks (3 s @ 20 Hz) of becoming stale.
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        if faction.member_count == 0 {
            continue;
        }
        if (fid as u64).wrapping_add(tick) % 60 != 0 {
            continue;
        }

        let new_hash = culture_hash(faction);
        let needs_plan = match plans.0.get(&fid) {
            Some(p) => {
                p.zones.is_empty()
                    || p.culture_hash != new_hash
                    || tick.saturating_sub(p.planned_at_tick) > REPLAN_INTERVAL
            }
            None => true,
        };
        if !needs_plan {
            continue;
        }
        let plan = build_settlement_plan(fid, faction, tick);
        plans.0.insert(fid, plan);
    }
}
