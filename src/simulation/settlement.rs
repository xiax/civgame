//! Per-faction settlement plans: zoned regions around the home tile that
//! direct where buildings of a given kind may be placed.
//!
//! The planner system that populates these is added in Phase 2; this module
//! defines the data model so other systems (debug panel, build selector) can
//! reference the types now.
//!
//! Pluralist Economy R1: this module also owns the `Settlement` entity —
//! the *economic* unit (market + treasury + market_tile). A faction can
//! own multiple settlements (colonies), and a megachunk can host
//! settlements from multiple competing factions. `SettlementPlan` (above)
//! is the *layout* of buildings around a hearth and is keyed per-faction;
//! `Settlement` (below) is the economic unit and has its own ID space.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::market::SettlementMarket;
use crate::simulation::faction::{FactionData, FactionRegistry, LayoutStyle, PlayerFaction, SOLO};
use crate::simulation::schedule::SimClock;
use crate::simulation::technology::{
    current_era, Era, CROP_CULTIVATION, FLINT_KNAPPING, LONG_DIST_TRADE, PERM_SETTLEMENT,
    SACRED_RITUAL,
};
use crate::world::terrain::TILE_SIZE;

// ─── Pluralist Economy R1: Settlement entity ────────────────────────

/// Stable per-settlement identity, allocated by `SettlementMap::alloc_id`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct SettlementId(pub u32);

/// A settlement is owned by exactly one faction. A megachunk can host
/// many competing settlements (different factions, different
/// ideologies). `market_tile` is the canonical access point for trade;
/// `treasury` is the settlement-level currency pool that funds public
/// works (R5+); `market` is the per-settlement Walrasian price/supply/
/// demand state (activated in R7 — until then it's seeded empty and
/// idle).
#[derive(Component, Clone, Debug)]
pub struct Settlement {
    pub id: SettlementId,
    pub owner_faction: u32,
    pub market_tile: (i32, i32),
    pub founding_tick: u64,
    pub name: String,
    pub treasury: f32,
    pub market: SettlementMarket,
    /// Highest `owner_faction.member_count` ever observed. Civic milestone
    /// gates read this so a settlement that drops 30 → 15 doesn't lose its
    /// market. Maintained by `settlement_peak_population_system`.
    pub peak_population: u32,
}

/// Resource indexing every live `Settlement` entity by id, megachunk,
/// and owner faction. Maintained by the auto-found system + future
/// chief/player-driven settlement spawning.
#[derive(Resource, Default)]
pub struct SettlementMap {
    pub by_id: AHashMap<SettlementId, Entity>,
    pub by_megachunk: AHashMap<(i32, i32), Vec<SettlementId>>,
    pub by_faction: AHashMap<u32, Vec<SettlementId>>,
    pub next_id: u32,
}

impl SettlementMap {
    pub fn alloc_id(&mut self) -> SettlementId {
        let id = SettlementId(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn register(
        &mut self,
        id: SettlementId,
        entity: Entity,
        megachunk: (i32, i32),
        owner_faction: u32,
    ) {
        self.by_id.insert(id, entity);
        self.by_megachunk.entry(megachunk).or_default().push(id);
        self.by_faction.entry(owner_faction).or_default().push(id);
    }

    /// First settlement registered under `faction_id`, or None.
    pub fn first_for_faction(&self, faction_id: u32) -> Option<SettlementId> {
        self.by_faction
            .get(&faction_id)
            .and_then(|v| v.first().copied())
    }

    /// Every settlement registered under `faction_id`.
    pub fn for_faction(&self, faction_id: u32) -> &[SettlementId] {
        self.by_faction
            .get(&faction_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

/// Auto-found a default settlement for any non-SOLO faction that has a
/// known `home_tile` but no settlement registered yet. Runs every
/// Sequential tick — cheap because most factions already have one.
///
/// This keeps the existing 287 tests green: the test fixture creates a
/// player faction at `(0, 0)` and assumes faction-storage / chief
/// systems work as before. Once auto-founding has run for one tick,
/// the faction has a Settlement entity at its home tile that future
/// phases can attach treasury / market / job-board behavior to without
/// retrofitting every existing test.
pub fn auto_found_default_settlements_system(
    mut commands: Commands,
    mut map: ResMut<SettlementMap>,
    registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
) {
    for (faction_id, data) in registry.factions.iter() {
        if *faction_id == SOLO {
            continue;
        }
        if map.by_faction.contains_key(faction_id) {
            continue;
        }
        // Abstract world-map factions have no entities yet — they only found
        // a Settlement when the player travels near and materialises them.
        if !data.materialized {
            continue;
        }
        // Nomadic factions skip Settlement creation entirely. They have no
        // permanent market_tile, no plots, no treasury — `home_tile` is a
        // mutable camp anchor, and storage pools across member/pack-animal
        // inventories (Phase 4 backend split). See nomadic-mode plan.
        // Capability check: only FullSettlement archetypes auto-found a Settlement.
        if !data.caps.settlement.is_full_settlement() {
            continue;
        }
        let home = data.home_tile;
        let mc = crate::simulation::region::MegaChunkCoord::from_tile(home.0, home.1);
        let id = map.alloc_id();
        let entity = commands
            .spawn(Settlement {
                id,
                owner_faction: *faction_id,
                market_tile: home,
                founding_tick: clock.tick,
                name: format!("Settlement {}", id.0),
                treasury: 0.0,
                market: SettlementMarket::default(),
                peak_population: data.member_count,
            })
            .id();
        map.register(id, entity, mc, *faction_id);
    }
}

/// Bumps each `Settlement.peak_population` to the current `member_count` of
/// its owner faction. Civic-milestone gates (Phase 5) read peak so a tribe
/// that drops 30 → 15 keeps its market. Cheap — one read + conditional write
/// per Settlement per tick.
pub fn settlement_peak_population_system(
    mut settlements: Query<&mut Settlement>,
    registry: Res<FactionRegistry>,
) {
    for mut s in settlements.iter_mut() {
        let Some(data) = registry.factions.get(&s.owner_faction) else {
            continue;
        };
        if data.member_count > s.peak_population {
            s.peak_population = data.member_count;
        }
    }
}

/// Inclusive-exclusive rectangle in tile coordinates: tiles `(x, y)` with
/// `x0 <= x < x0+w`, `y0 <= y < y0+h`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRect {
    pub x0: i32,
    pub y0: i32,
    pub w: u16,
    pub h: u16,
}

impl TileRect {
    pub fn new(x0: i32, y0: i32, w: u16, h: u16) -> Self {
        Self { x0, y0, w, h }
    }

    #[inline]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0
            && y >= self.y0
            && (x as i32) < self.x0 as i32 + self.w as i32
            && (y as i32) < self.y0 as i32 + self.h as i32
    }

    pub fn center(&self) -> (i32, i32) {
        (
            (self.x0 as i32 + self.w as i32 / 2) as i32,
            (self.y0 as i32 + self.h as i32 / 2) as i32,
        )
    }

    pub fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }
}

/// Functional category of a zone within a settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Tier of a carved street segment. Primary spines run between plazas; Secondary
/// branches off the spine at residential clusters; Alley threads back lots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreetTier {
    Primary,
    Secondary,
    Alley,
}

/// One carved street segment in tile coordinates. End-inclusive Bresenham line
/// from `start` to `end`. Consumed by `spine_carve_system` (Phase 1) which
/// drains segments into `RoadCarveQueue` chunk-by-chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreetSegment {
    pub start: (i32, i32),
    pub end: (i32, i32),
    pub tier: StreetTier,
}

/// Per-settlement street-spine plan. Generated by Phase 1's
/// `generate_streetspine` and stamped onto `SettlementPlan` once the spine type
/// is wired through. Variant choice is driven by `LayoutStyle` × `Era`:
/// Paleo/Meso get `None` (radial behavior preserved); Linear/Compact + Neo+ get
/// `Linear`; Citadel/Sprawling + Chalc+ get `Grid`. Radial layouts always get
/// `Spokes`.
#[derive(Clone, Debug, Default)]
pub enum StreetSpine {
    /// No carved streets — radial / pre-settlement. Default for all factions
    /// until Phase 1 generation lands.
    #[default]
    None,
    /// Cardinal/diagonal spokes radiating from the plaza node.
    Spokes {
        plaza: (i32, i32),
        segments: Vec<StreetSegment>,
    },
    /// Single primary spine through the plaza node. Used by Linear/Compact
    /// layouts in Neolithic+.
    Linear {
        plaza: (i32, i32),
        segments: Vec<StreetSegment>,
    },
    /// Primary + perpendicular secondary branches at population thresholds.
    /// Used by Citadel/Sprawling layouts in Chalcolithic+.
    Grid {
        plaza: (i32, i32),
        segments: Vec<StreetSegment>,
    },
}

impl StreetSpine {
    /// All carved segments, regardless of variant. Empty for `None`.
    pub fn segments(&self) -> &[StreetSegment] {
        match self {
            StreetSpine::None => &[],
            StreetSpine::Spokes { segments, .. }
            | StreetSpine::Linear { segments, .. }
            | StreetSpine::Grid { segments, .. } => segments,
        }
    }

    /// Plaza node, if any. Phase 1+2 moves `Settlement.market_tile` here.
    pub fn plaza(&self) -> Option<(i32, i32)> {
        match self {
            StreetSpine::None => None,
            StreetSpine::Spokes { plaza, .. }
            | StreetSpine::Linear { plaza, .. }
            | StreetSpine::Grid { plaza, .. } => Some(*plaza),
        }
    }
}

/// Per-faction settlement plan. Re-evaluated periodically by the planner; the
/// build selector consumes it to decide where to place new blueprints.
#[derive(Clone, Debug, Default)]
pub struct SettlementPlan {
    pub zones: Vec<Zone>,
    /// Carved street geometry. Generated by `generate_streetspine` from
    /// `LayoutStyle` × `Era`. Pre-Neolithic factions get `StreetSpine::None`
    /// and behave radially as before. `settlement_planner_system` enqueues
    /// segments into `RoadCarveQueue` when the plan is (re)computed.
    pub spine: StreetSpine,
    /// Tick at which the plan was last (re)computed. 0 = never planned.
    pub planned_at_tick: u64,
    /// Hash of inputs that, if changed, force a re-plan
    /// (`techs.0 + member_count_bucket + culture.style`).
    pub culture_hash: u64,
}

impl SettlementPlan {
    pub fn zone_for(&self, kind: ZoneKind, x: i32, y: i32) -> Option<&Zone> {
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
    let x0 = x0.clamp(i32::MIN as i32, i32::MAX as i32) as i32;
    let y0 = y0.clamp(i32::MIN as i32, i32::MAX as i32) as i32;
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
    home: (i32, i32),
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

/// River-aware overlay on `paleolithic_hearth_positions`. Each radial
/// candidate is run through `river_context::project_to_safe_bank` so
/// hearths never land in rivers or across the channel from `home`. Tiles
/// at `river_distance_at <= 1` are also rejected (flood band). Falls back
/// to the raw radial position when no projection succeeds within the
/// spiral radius — keeps Paleolithic seeding deterministic in fixtures
/// where rivers aren't present.
pub fn paleolithic_hearth_positions_river_aware(
    chunk_map: &crate::world::chunk::ChunkMap,
    faction_id: u32,
    home: (i32, i32),
    members: u32,
) -> Vec<(i32, i32)> {
    let raw = paleolithic_hearth_positions(faction_id, home, members);
    raw.into_iter()
        .map(|c| {
            // Reject flood-band tiles before projection so a projected
            // candidate that happens to sit at distance 1 also fails.
            let too_close = chunk_map.river_distance_at(c.0, c.1) <= 1;
            let needs_projection = too_close
                || chunk_map
                    .tile_kind_at(c.0, c.1)
                    .map(|k| k.is_water_like() || !k.is_passable())
                    .unwrap_or(false);
            if needs_projection {
                crate::simulation::river_context::project_to_safe_bank(chunk_map, c, home)
                    .unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Era-parametrized streetspine generation. Pre-Neolithic factions stay
/// `None` (radial behavior preserved). Neolithic+ generates spine geometry
/// scaled to `base_r`. The plaza node is the home tile so the spine doesn't
/// bisect the seeded campfire / market_tile.
///
/// **Layout × Era:**
/// - Paleolithic / Mesolithic → `None` (any style)
/// - Radial (Neo+) → `Spokes` (cardinal NS + EW)
/// - Linear / Compact (Neo+) → `Linear` (single E-W spine)
/// - Sprawling / Citadel (Neo+) → `Spokes` (cardinal NS + EW)
/// - Sprawling / Citadel (Chalc+) → `Grid` (cardinal NS + EW + diagonals)
pub fn generate_streetspine(
    home: (i32, i32),
    style: LayoutStyle,
    era: Era,
    base_r: i32,
) -> StreetSpine {
    if (era as u8) < (Era::Neolithic as u8) {
        return StreetSpine::None;
    }
    let plaza = home;
    let r = base_r.max(6);

    match style {
        LayoutStyle::Linear | LayoutStyle::Compact => {
            // Single E-W primary through the plaza.
            let segments = vec![StreetSegment {
                start: (home.0 - r, home.1),
                end: (home.0 + r, home.1),
                tier: StreetTier::Primary,
            }];
            StreetSpine::Linear { plaza, segments }
        }
        LayoutStyle::Sprawling | LayoutStyle::Citadel
            if (era as u8) >= (Era::Chalcolithic as u8) =>
        {
            // Grid: NS + EW primaries + two diagonals as Secondary.
            let r2 = (r * 7) / 10; // diagonals shorter
            let segments = vec![
                StreetSegment {
                    start: (home.0, home.1 - r),
                    end: (home.0, home.1 + r),
                    tier: StreetTier::Primary,
                },
                StreetSegment {
                    start: (home.0 - r, home.1),
                    end: (home.0 + r, home.1),
                    tier: StreetTier::Primary,
                },
                StreetSegment {
                    start: (home.0 - r2, home.1 - r2),
                    end: (home.0 + r2, home.1 + r2),
                    tier: StreetTier::Secondary,
                },
                StreetSegment {
                    start: (home.0 - r2, home.1 + r2),
                    end: (home.0 + r2, home.1 - r2),
                    tier: StreetTier::Secondary,
                },
            ];
            StreetSpine::Grid { plaza, segments }
        }
        // Radial / Sprawling-Neo / Citadel-Neo all get cardinal spokes.
        _ => {
            let segments = vec![
                StreetSegment {
                    start: (home.0, home.1 - r),
                    end: (home.0, home.1 + r),
                    tier: StreetTier::Primary,
                },
                StreetSegment {
                    start: (home.0 - r, home.1),
                    end: (home.0 + r, home.1),
                    tier: StreetTier::Primary,
                },
            ];
            StreetSpine::Spokes { plaza, segments }
        }
    }
}

/// Build a fresh `SettlementPlan` for a faction, choosing zone shapes from
/// the faction's `LayoutStyle` and tech progression. Procedural — no terrain
/// scoring yet (Phase 3+).
pub fn build_settlement_plan(faction_id: u32, faction: &FactionData, tick: u64) -> SettlementPlan {
    let (hx, hy) = (faction.home_tile.0 as i32, faction.home_tile.1 as i32);
    let style = faction.culture.style;
    let members = faction.member_count.max(2);
    // Civic / tier gates inside the planner answer "what does the
    // community currently build?" — query the community-adoption layer,
    // not chief-Aware. `faction.techs` (chief-Aware) is reserved for
    // planning-authority checks (`can_direct_tech`) elsewhere.
    let community_techs =
        crate::simulation::technology_adoption::community_adoption_bitset(faction);
    let techs = &community_techs;

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

    let spine = generate_streetspine(faction.home_tile, style, current_era(techs), base_r);

    // Layout-seed organic jitter: ±1 tile offset per non-civic / non-defense
    // zone rect, deterministic from culture_hash so re-derivation produces
    // identical layouts. Two factions with different ids produce visibly
    // different residential / crafting / storage arrangements at the same
    // (era, members, style).
    let seed = culture_hash(faction);
    jitter_zones(&mut zones, seed);

    SettlementPlan {
        zones,
        spine,
        planned_at_tick: tick,
        culture_hash: seed,
    }
}

/// Apply ±1 tile deterministic jitter to non-structural zone rects so two
/// factions with the same `(era, members, style)` produce different layouts.
/// Civic / Defense / Sacred zones stay fixed — they anchor the rest of the
/// plan and breaking their relative positions causes visual chaos.
fn jitter_zones(zones: &mut [Zone], seed: u64) {
    let mut rng = fastrand::Rng::with_seed(seed);
    for zone in zones.iter_mut() {
        match zone.kind {
            ZoneKind::Civic | ZoneKind::Defense | ZoneKind::Sacred => continue,
            _ => {}
        }
        let jx = (rng.i32(-1..=1)) as i32;
        let jy = (rng.i32(-1..=1)) as i32;
        zone.rect.x0 += jx;
        zone.rect.y0 += jy;
    }
}

/// Hearth-and-cluster layout for pre-settlement bands: a small Civic anchor
/// for each hearth plus a per-hearth Residential bbox that the build selector
/// uses to cluster sleeping spots around the fire.
fn build_paleolithic_plan(faction_id: u32, faction: &FactionData, tick: u64) -> SettlementPlan {
    let (hx, hy) = (faction.home_tile.0 as i32, faction.home_tile.1 as i32);
    let members = faction.member_count.max(1);
    // See `build_settlement_plan` — civic zone gates query community
    // adoption, not chief-Aware.
    let community_techs =
        crate::simulation::technology_adoption::community_adoption_bitset(faction);
    let techs = &community_techs;

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
        spine: StreetSpine::None,
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
    projector: crate::rendering::projection::LogicalProjector,
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
            let center = projector.project(Vec2::new(cx, cy));
            // Width in projected space matches logical (X is preserved);
            // height compresses with `y_scale`. Approximate by re-projecting
            // the four corners and taking their bounding box — handles cases
            // where the zone spans multiple elevation tiers and avoids a
            // hard `y_scale` constant here.
            let tl = projector.project(Vec2::new(x_min, y_min));
            let tr = projector.project(Vec2::new(x_max, y_min));
            let bl = projector.project(Vec2::new(x_min, y_max));
            let br = projector.project(Vec2::new(x_max, y_max));
            let proj_min = tl.min(tr).min(bl).min(br);
            let proj_max = tl.max(tr).max(bl).max(br);
            let size = proj_max - proj_min;
            gizmos.rect_2d(
                Isometry2d::from_translation(center),
                size,
                zone_color(zone.kind),
            );
        }
    }
}

/// Shared planning core. Computes the `(plan, new_hash)` pair for one
/// faction, preferring an organic `SettlementBrain` projection when one
/// exists and falling back to `build_settlement_plan`. Caller is expected to
/// have filtered out SOLO / memberless factions; the OnEnter wrapper iterates
/// every eligible faction once, while `settlement_planner_system` runs it
/// inside its stagger + `needs_plan` gate.
pub fn project_plan_for_faction(
    fid: u32,
    faction: &FactionData,
    tick: u64,
    settlement_map: &SettlementMap,
    brains: &crate::simulation::organic_settlement::SettlementBrains,
) -> (SettlementPlan, u64) {
    let organic_brain = settlement_map
        .first_for_faction(fid)
        .and_then(|sid| brains.0.get(&sid));
    let new_hash = organic_brain
        .map(|brain| brain.layout_hash ^ ((fid as u64) << 32))
        .unwrap_or_else(|| culture_hash(faction));
    let plan = organic_brain
        .map(|brain| {
            crate::simulation::organic_settlement::compat_plan_from_brain(fid, faction, tick, brain)
        })
        .unwrap_or_else(|| build_settlement_plan(fid, faction, tick));
    (plan, new_hash)
}

/// OnEnter(Playing) one-shot: project a `SettlementPlan` for every non-SOLO
/// member-bearing faction so `carve_plots_system` can run inside the
/// `OnEnter` chain and own all tick-0 plots (`PlotIndex.by_faction_hash`
/// established before any runtime stagger can fire). Does **not** push spine
/// segments onto `RoadCarveQueue` — OnEnter road carving is already owned by
/// `kickoff_initial_survey_system` + `seed_starting_buildings_system`.
pub fn project_initial_settlement_plans_system(
    registry: Res<FactionRegistry>,
    settlement_map: Res<SettlementMap>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
    mut plans: ResMut<SettlementPlans>,
) {
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO || faction.member_count == 0 {
            continue;
        }
        let (plan, _new_hash) =
            project_plan_for_faction(fid, faction, 0, &settlement_map, &brains);
        plans.0.insert(fid, plan);
    }
}

/// System: re-evaluates each non-SOLO faction's settlement plan periodically.
/// Throttled — at most one faction is re-planned per tick to spread CPU cost.
/// On a culture-hash bump (i.e. a fresh spine geometry), enqueues every
/// `StreetSegment` into `RoadCarveQueue` so `road_carve_system` carves them
/// over the next several ticks.
pub fn settlement_planner_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlement_map: Res<SettlementMap>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
    mut plans: ResMut<SettlementPlans>,
    mut road_queue: ResMut<crate::simulation::construction::RoadCarveQueue>,
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

        // Compute candidate plan + hash via the shared helper before the
        // `needs_plan` gate so OnEnter and runtime read the same projection.
        let (plan, new_hash) =
            project_plan_for_faction(fid, faction, tick, &settlement_map, &brains);
        let prev_hash = plans.0.get(&fid).map(|p| p.culture_hash);
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

        // Enqueue spine carving once per culture_hash bump. Reuses
        // `RoadCarveQueue` (one Bresenham line per drained entry) — segments
        // share the same Sequential drain cadence as building→home roads.
        if prev_hash != Some(new_hash) {
            for seg in plan.spine.segments() {
                road_queue.0.push((fid, seg.start, seg.end));
            }
        }

        plans.0.insert(fid, plan);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk(kind: TileKind) -> Chunk {
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[8u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    fn flat_map_with_river(river_x: i32) -> ChunkMap {
        let mut m = ChunkMap::default();
        for cy in -1..=1 {
            for cx in -1..=1 {
                m.0.insert(ChunkCoord(cx, cy), flat_chunk(TileKind::Grass));
            }
        }
        for y in -30..=30 {
            m.set_tile(
                river_x,
                y,
                0,
                TileData {
                    kind: TileKind::River,
                    ..Default::default()
                },
            );
        }
        m
    }

    #[test]
    fn paleo_river_aware_keeps_hearths_off_river() {
        // Home is east of an NS river. Project should not return any
        // hearth on a river tile, and all hearths should be passable.
        let m = flat_map_with_river(0);
        let hearths = paleolithic_hearth_positions_river_aware(&m, 1, (4, 0), 6);
        assert!(!hearths.is_empty());
        for (hx, hy) in hearths {
            let kind = m.tile_kind_at(hx, hy).unwrap();
            assert!(
                kind.is_passable() && !kind.is_water_like(),
                "hearth on bad tile {:?} (kind={:?})",
                (hx, hy),
                kind
            );
        }
    }
}
