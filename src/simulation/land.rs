//! Land ownership — Phase 1.
//!
//! Carves rectangular `Plot` entities out of every settlement's
//! `SettlementPlan` zones. All plots start `Tenure::StateOwned` held by
//! the settlement's owning faction. Valuation, listings, leasing, and
//! freehold transfers are layered on in later phases — this module only
//! supplies the data model and the carving hook for now.
//!
//! See `~/.claude/plans/i-want-to-add-starry-conway.md` for the full
//! plan.

use ahash::{AHashMap, AHashSet};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::simulation::faction::{FactionRegistry, SOLO};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{
    Settlement, SettlementMap, SettlementPlans, StreetSegment, StreetSpine, TileRect, ZoneKind,
};
use crate::world::chunk::{ChunkMap, Z_MIN};
#[allow(unused_imports)]
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::Globe;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::terrain::{tile_at_3d, WorldGen};
#[allow(unused_imports)]
use crate::world::tile::{TileData, TileKind};

pub type PlotId = u32;

/// Settlement realism: per-tile role within a 16×16 Agricultural plot.
/// Keyed deterministically on `(culture_seed, faction_id, tile)` so the
/// same world rerolls identical field mosaics. Every variant stays in
/// `PlotIndex.ag_tiles` (so `tile_is_farm_protected` still blocks road
/// carving over the whole 256-tile rect) and stays plantable
/// (`plants::seed_target_tile_ok` accepts Grass + soil_like + Cropland).
///
/// Distribution targets: 60% Cropland / 15% CroplandLow / 20% SoilFallow
/// / 5% GrassEdge — perimeter tiles bias toward GrassEdge so the field
/// silhouette softens from a perfect golden square into a mottled blob
/// with stubble at the edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldTileRole {
    /// Planted rows, fertility 200 if current is lower.
    Cropland,
    /// Recently harvested, lower fertility floor (~100).
    CroplandLow,
    /// Tilled-but-not-cropped — keep the underlying soil/loam tile.
    SoilFallow,
    /// Unimproved field edge — leave Grass / underlying terrain.
    GrassEdge,
}

#[inline]
fn field_splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Returns the deterministic role for a tile inside a plot whose
/// bounding box is `(x0, y0, w, h)`. Edge tiles (cheb to the perimeter
/// = 0) bias toward `GrassEdge` so the field outline softens.
pub fn field_tile_role(
    culture_seed: u64,
    faction_id: u32,
    tile: (i32, i32),
    rect: TileRect,
) -> FieldTileRole {
    let mut mix = field_splitmix64(
        culture_seed
            ^ ((faction_id as u64) << 1)
            ^ ((tile.0 as u32 as u64) << 32)
            ^ (tile.1 as u32 as u64),
    );
    // Stir once more so faction_id contributes to bits other than the LSB.
    mix = field_splitmix64(mix);
    let mut roll = (mix as f32 / u64::MAX as f32).clamp(0.0, 1.0);
    // Perimeter bias: on the outermost ring of the rect, push the roll
    // higher so the GrassEdge branch (top tail) wins more often.
    let on_edge_x = tile.0 == rect.x0 || tile.0 == rect.x0 + rect.w as i32 - 1;
    let on_edge_y = tile.1 == rect.y0 || tile.1 == rect.y0 + rect.h as i32 - 1;
    if on_edge_x || on_edge_y {
        roll = (roll + 0.30).min(1.0);
    }
    if roll < 0.60 {
        FieldTileRole::Cropland
    } else if roll < 0.75 {
        FieldTileRole::CroplandLow
    } else if roll < 0.95 {
        FieldTileRole::SoilFallow
    } else {
        FieldTileRole::GrassEdge
    }
}

/// Which side of a `Plot.rect` faces a carved street. Phase 2 of the
/// Construction Overhaul: lot-driven placement (Phase 3) anchors footprints
/// at the frontage edge so structures face their road. Doubles as the cardinal
/// for a door's opening side: `pick_door_direction` derives a `TileEdge` from
/// the plot's frontage (or a road halo / home cardinal fallback), and the door
/// is placed on that side of the building. The doormat tile sits one step in
/// the same direction outside the building.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileEdge {
    North,
    East,
    South,
    West,
}

impl TileEdge {
    /// Unit offset for "one step outward in this cardinal direction".
    pub fn delta(self) -> (i32, i32) {
        match self {
            TileEdge::North => (0, 1),
            TileEdge::South => (0, -1),
            TileEdge::East => (1, 0),
            TileEdge::West => (-1, 0),
        }
    }

    /// Cardinal direction from `from` toward `to` (Chebyshev). Ties prefer
    /// the axis with the larger absolute delta; pure-diagonal inputs prefer
    /// East/West for x-positive, North/South otherwise.
    pub fn toward(from: (i32, i32), to: (i32, i32)) -> TileEdge {
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        if dx.abs() >= dy.abs() {
            if dx >= 0 {
                TileEdge::East
            } else {
                TileEdge::West
            }
        } else if dy >= 0 {
            TileEdge::North
        } else {
            TileEdge::South
        }
    }
}

/// A bounded rectangular parcel of land within a settlement. Tenure
/// captures *how* the holder relates to the land (state, lease,
/// sharecrop, freehold); holder identifies *who* benefits.
#[derive(Component, Clone, Debug)]
pub struct Plot {
    pub id: PlotId,
    pub settlement_id: u32,
    pub faction_id: u32,
    pub rect: TileRect,
    pub z: i8,
    pub zone_kind: ZoneKind,
    pub tenure: Tenure,
    pub holder: TenureHolder,
    /// Currency-denominated valuation. Recomputed event-driven (Phase 2+);
    /// 0.0 in Phase 1.
    pub base_value: f32,
    pub last_valued_tick: u64,
    pub missed_payments: u8,
    /// Side of `rect` that faces the nearest carved street segment, if any.
    /// Populated by `carve_plots_system` from `SettlementPlan.spine`.
    /// `None` for radial / pre-Neolithic settlements without a spine.
    pub frontage_edge: Option<TileEdge>,
    /// The exact road tile this plot opens onto, if `frontage_edge.is_some()`.
    /// Lot-driven placement (Phase 3) anchors footprint search at the edge
    /// adjacent to this tile.
    pub access_tile: Option<(i32, i32)>,
    /// Phase 6: when this plot is a household-attached subordinate (typically
    /// an Agricultural strip claimed alongside a Residential lot), `parent_plot`
    /// names the residential plot it belongs to. Child plots are skipped by
    /// `land_listing_system` (not separately for sale / lease) and by
    /// `rent_collection_system` (rent flows through the parent's tenure).
    pub parent_plot: Option<PlotId>,
    /// Draftwork v2: the calendar year this plot was last tilled by a draft
    /// animal (`Calendar.year as u16`). `Some(y)` means crops planted this same
    /// year sprout with a `Tilled` marker and harvest at `PLOW_YIELD_MULT`
    /// (1.4×) of the nutrient-tier base. Reset by re-plow next spring; no
    /// explicit clear needed since planting checks year equality.
    pub plowed_year: Option<u16>,
}

/// What kind of relationship the holder has with the plot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tenure {
    /// Held by the settlement / state. Default for newly-carved plots.
    StateOwned,
    /// Tenant pays a periodic currency rent.
    Leased {
        rent_per_month: f32,
        period_days: u32,
        paid_through_tick: u64,
    },
    /// Tenant farms the plot and pays a fraction of the harvest.
    Sharecropping {
        share_to_landlord: f32,
        paid_through_tick: u64,
    },
    /// Outright ownership; transferable.
    Freehold,
}

/// Identifies the beneficiary of the tenure. State plots route proceeds
/// to the settlement / faction treasury; household plots to the
/// sub-faction treasury.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TenureHolder {
    State { faction_id: u32 },
    Household { faction_id: u32 },
    // Future variants (aristocrat, individual, commons) slot in here.
}

impl TenureHolder {
    /// The faction whose treasury collects rent / receives sale proceeds
    /// or owns the plot's improvements.
    pub fn faction_id(self) -> u32 {
        match self {
            TenureHolder::State { faction_id } => faction_id,
            TenureHolder::Household { faction_id } => faction_id,
        }
    }
}

/// Per-faction registry of carved plots. `by_tile` is the primary
/// lookup hot path — `tile_buildable_by` and the future farm-job gate
/// both index here. `by_faction_hash` lets the carve system skip work
/// when the upstream `SettlementPlan` hasn't changed.
#[derive(Resource, Default)]
pub struct PlotIndex {
    pub by_id: AHashMap<PlotId, Entity>,
    pub by_settlement: AHashMap<u32, Vec<PlotId>>,
    /// Plots are conceptually 2D surface regions. The `Plot.z` field
    /// records the layer (always 0 in Phase 1; future tunneling-aware
    /// plots will use distinct values), but the lookup key is `(x, y)`
    /// — there's only ever one surface plot at a given column. When
    /// underground plots arrive, give them their own index alongside.
    pub by_tile: AHashMap<(i32, i32), PlotId>,
    /// Surface tiles belonging to an `Agricultural` plot. Maintained in
    /// lockstep with `by_tile` by `carve_plots_system`; lets
    /// `tile_is_farm_protected` answer the road-carve guard in O(1) without
    /// resolving the `Plot` component (so `road_carve_system` /
    /// `write_road_tile` need only `Res<PlotIndex>`, not a `Query<&Plot>`).
    pub ag_tiles: AHashSet<(i32, i32)>,
    pub by_faction_hash: AHashMap<u32, u64>,
    pub next_id: u32,
}

impl PlotIndex {
    pub fn alloc_id(&mut self) -> PlotId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// `PlotId` covering the given surface tile, if any.
    pub fn plot_at(&self, x: i32, y: i32) -> Option<PlotId> {
        self.by_tile.get(&(x, y)).copied()
    }
}

/// True when `tile` must never be overwritten by road carving — it is inside
/// an `Agricultural` plot or carries a planted crop. The single predicate
/// behind the `road_carve_system` / `write_road_tile` guard, so every
/// `RoadCarveQueue` producer is protected at one chokepoint.
pub fn tile_is_farm_protected(
    plot_index: &PlotIndex,
    plant_map: &crate::simulation::plants::PlantMap,
    tile: (i32, i32),
) -> bool {
    plot_index.ag_tiles.contains(&tile) || plant_map.0.contains_key(&tile)
}

/// Open listings for state-owned (and, eventually, household-owned)
/// plots. Populated by the listing system in Phase 4+; defined now so
/// downstream systems can reference it without a follow-up wire-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingKind {
    Sale,
    Lease,
    Sharecrop,
}

#[derive(Clone, Debug)]
pub struct Listing {
    pub plot_id: PlotId,
    pub asking: f32,
    pub kind: ListingKind,
    pub listed_tick: u64,
    pub unsold_days: u16,
}

#[derive(Resource, Default)]
pub struct LandListings {
    pub for_sale: Vec<Listing>,
    pub for_lease: Vec<Listing>,
}

impl LandListings {
    pub fn is_listed(&self, plot_id: PlotId) -> bool {
        self.for_sale.iter().any(|l| l.plot_id == plot_id)
            || self.for_lease.iter().any(|l| l.plot_id == plot_id)
    }
}

/// Phase 4 tunables. Listings cap keeps the open-market list bounded
/// so a faction with hundreds of carved plots doesn't dump all of them
/// at once. The affordability ratios stop a household from blowing its
/// entire treasury on housing in one tick.
///
/// Pre-farm-planner this was a single global cap (8) shared across all
/// zone kinds, which let agricultural plots crowd out residential when
/// they triple-listed (Sale + Lease + Sharecrop). Now per-zone so each
/// market track gets its own budget — counted by `plot_id`, not row, so
/// one plot triple-listing still consumes only one slot.
pub const TARGET_LISTINGS_PER_FACTION: usize = 8;
pub const RESIDENTIAL_LISTINGS_CAP: usize = 6;
pub const AGRICULTURAL_LISTINGS_CAP: usize = 6;
pub const CRAFTING_LISTINGS_CAP: usize = 4;

/// Per-faction, per-zone-kind listing budget. `Civic / Sacred / Market /
/// Defense` are not listed at all (handled by the early-skip at the top of
/// `land_listing_system`).
fn listings_cap_for(zone: ZoneKind) -> usize {
    match zone {
        ZoneKind::Residential => RESIDENTIAL_LISTINGS_CAP,
        ZoneKind::Agricultural => AGRICULTURAL_LISTINGS_CAP,
        ZoneKind::Crafting | ZoneKind::Storage => CRAFTING_LISTINGS_CAP,
        // Public-works zones — never listed.
        ZoneKind::Civic | ZoneKind::Sacred | ZoneKind::Market | ZoneKind::Defense => 0,
    }
}
pub const HOUSEHOLD_MIN_TREASURY_FOR_LEASE: f32 = 5.0;
pub const HOUSEHOLD_LEASE_AFFORDABILITY: f32 = 0.40;
pub const HOUSEHOLD_BUY_AFFORDABILITY: f32 = 0.70;
pub const MIN_MONTHLY_RENT: f32 = 0.5;
/// Phase 5: number of consecutive missed rent cycles after which a
/// tenant is evicted. Two months matches the historical lease grace
/// window — gives a household one chance to recover before losing the
/// plot.
pub const EVICTION_MISS_THRESHOLD: u8 = 2;

/// P7b: emitted by `rent_collection_system` on every eviction. The
/// landlord's `caps.land.eviction_policy` rides on the event so a
/// downstream cleanup system can act on it without re-reading the
/// `FactionRegistry`. `LeaveStructures` events are still emitted (for
/// observability / activity-log hooks); the cleanup system simply
/// no-ops on them.
#[derive(Event, Clone, Copy, Debug)]
pub struct PlotEvictedEvent {
    pub plot_entity: Entity,
    pub plot_id: PlotId,
    pub plot_rect: TileRect,
    pub plot_z: i8,
    pub landlord_faction: u32,
    pub policy: crate::simulation::archetype::EvictionPolicy,
}

/// Phase 6: bundled inputs to the gather harvest hook. `gather_system`
/// already sits at Bevy's 16-param ceiling (it bundles routing
/// resources too), so the sharecrop look-up + landlord-share drop ride
/// inside one `SystemParam`.
#[derive(SystemParam)]
pub struct SharecropResources<'w, 's> {
    pub plot_index: Res<'w, PlotIndex>,
    pub plot_q: Query<'w, 's, &'static Plot>,
    pub spatial: Res<'w, crate::world::spatial::SpatialIndex>,
    pub item_q: Query<'w, 's, &'static mut crate::simulation::items::GroundItem>,
}

/// Base currency value used as the multiplicative anchor in
/// `compute_plot_value`. Tuned so a typical surface residential plot
/// near a market lands in the 30–80 range, far outpacing Subsistence-
/// preset household treasuries (15.0 starting capital) so freehold
/// purchase requires accumulated earnings while leasing stays
/// accessible.
pub const PLOT_BASE_VALUE: f32 = 50.0;

/// Local Chebyshev distance — `htn::chebyshev_dist` is private to that
/// module. Inlined here so the valuation path doesn't pull in an HTN
/// dependency.
#[inline]
fn chebyshev_dist(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// For a plot rect, find the adjacent edge (and a specific road tile on
/// that edge) facing the nearest spine segment within `MAX_FRONTAGE_RANGE`
/// tiles. Returns `(None, None)` for plots beyond range or for spines that
/// don't run alongside any of the plot's four edges.
///
/// Algorithm: for each of the rect's four perimeter rows/columns, take the
/// midpoint and walk outward up to `MAX_FRONTAGE_RANGE` tiles. The first
/// tile that lies on any spine segment (within `SEGMENT_TOLERANCE` of the
/// Bresenham line) wins; the corresponding `TileEdge` is returned.
fn frontage_for_rect(
    rect: TileRect,
    spine: &StreetSpine,
) -> (Option<TileEdge>, Option<(i32, i32)>) {
    const MAX_FRONTAGE_RANGE: i32 = 4;
    let segments = spine.segments();
    if segments.is_empty() {
        return (None, None);
    }

    // Mid-tile of each edge (just outside the rect on that side).
    let cx = rect.x0 + rect.w as i32 / 2;
    let cy = rect.y0 + rect.h as i32 / 2;
    let candidates: [(TileEdge, (i32, i32), (i32, i32)); 4] = [
        (TileEdge::North, (cx, rect.y0 + rect.h as i32), (0, 1)),
        (TileEdge::South, (cx, rect.y0 - 1), (0, -1)),
        (TileEdge::East, (rect.x0 + rect.w as i32, cy), (1, 0)),
        (TileEdge::West, (rect.x0 - 1, cy), (-1, 0)),
    ];

    let mut best: Option<(i32, TileEdge, (i32, i32))> = None;
    for (edge, start, dir) in candidates.iter() {
        for step in 0..MAX_FRONTAGE_RANGE {
            let probe = (start.0 + dir.0 * step, start.1 + dir.1 * step);
            if segment_set_contains(segments, probe) {
                let dist = step;
                if best.as_ref().map_or(true, |(d, _, _)| dist < *d) {
                    best = Some((dist, *edge, probe));
                }
                break;
            }
        }
    }
    match best {
        Some((_, edge, tile)) => (Some(edge), Some(tile)),
        None => (None, None),
    }
}

/// True iff `tile` lies on any segment's Bresenham line (with 0 tolerance).
/// Cheap perpendicular-distance check per segment.
fn segment_set_contains(segments: &[StreetSegment], tile: (i32, i32)) -> bool {
    segments.iter().any(|s| point_on_segment(*s, tile))
}

/// Standard "is `p` on the integer Bresenham line from `a` to `b`?" check
/// using cross-product collinearity + bounding-box containment. Cheap;
/// avoids replaying Bresenham for every probe tile.
fn point_on_segment(seg: StreetSegment, p: (i32, i32)) -> bool {
    let (ax, ay) = seg.start;
    let (bx, by) = seg.end;
    let dx = bx - ax;
    let dy = by - ay;
    let px = p.0 - ax;
    let py = p.1 - ay;
    // Collinear? Cross-product must be zero.
    if dx * py - dy * px != 0 {
        return false;
    }
    // Within bounds?
    let in_x = (p.0 >= ax.min(bx)) && (p.0 <= ax.max(bx));
    let in_y = (p.1 >= ay.min(by)) && (p.1 <= ay.max(by));
    in_x && in_y
}

/// Compute the currency-denominated value of a plot. Inputs:
///
/// - **centre/home distance** — closer to the settlement market and
///   faction home is more desirable; falls off with Chebyshev distance.
/// - **zone kind multiplier** — Crafting and Market plots command a
///   premium; Agricultural plots are cheap (priced for bulk farmland
///   sale). Civic/Sacred/Defense use a generic state premium.
/// - **terrain factor** — sampled fertility across plot corners + centre.
///   Fertile plots are worth more; Agricultural plots reweight terrain
///   harder because farmland's productive value lives in fertility.
///
/// Workplace-proximity premium is deferred to a later phase (it needs
/// active job-posting awareness).
pub fn compute_plot_value(
    rect: TileRect,
    zone_kind: ZoneKind,
    faction_home: (i32, i32),
    market_tile: (i32, i32),
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
) -> f32 {
    let (cx, cy) = rect.center();
    let market_d = chebyshev_dist((cx, cy), market_tile) as f32;
    let home_d = chebyshev_dist((cx, cy), faction_home) as f32;
    let centre_factor = 1.0 / (1.0 + market_d * 0.05);
    let home_factor = 1.0 / (1.0 + home_d * 0.03);

    let zone_mul = match zone_kind {
        ZoneKind::Residential => 1.0,
        ZoneKind::Crafting => 1.4,
        ZoneKind::Storage => 0.9,
        ZoneKind::Agricultural => 0.6,
        ZoneKind::Market => 1.5,
        ZoneKind::Civic | ZoneKind::Sacred | ZoneKind::Defense => 1.2,
    };

    let terrain_factor = sample_terrain_factor(rect, zone_kind, chunk_map, gen, globe);
    PLOT_BASE_VALUE * zone_mul * (centre_factor + home_factor) * terrain_factor
}

/// Sample fertility at the plot's centre + four corners, average,
/// rescale to `[0.25, 1.0]`. Agricultural plots reweight: a fully fertile
/// farmland plot stays at 1.0 of its base, but a barren one collapses
/// closer to 0.13 (rather than 0.25 for non-farm zones).
fn sample_terrain_factor(
    rect: TileRect,
    zone_kind: ZoneKind,
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
) -> f32 {
    let (cx, cy) = rect.center();
    let x_far = rect.x0 + rect.w as i32 - 1;
    let y_far = rect.y0 + rect.h as i32 - 1;
    let samples: [(i32, i32); 5] = [
        (cx, cy),
        (rect.x0, rect.y0),
        (x_far, rect.y0),
        (rect.x0, y_far),
        (x_far, y_far),
    ];
    let mut sum: u32 = 0;
    let mut count: u32 = 0;
    for (x, y) in samples {
        // Rendered surface: `z` feeds `tile_at_3d` to sample kind/fertility,
        // so a wet tile must read the water surface (low plot value), not the
        // dry bed beneath it. Stays on `surface_z_at` (TOP_SURFACE).
        let z = chunk_map.surface_z_at(x, y);
        if z < Z_MIN {
            continue;
        }
        let tile = tile_at_3d(chunk_map, gen, globe, x, y, z);
        sum += tile.fertility as u32;
        count += 1;
    }
    if count == 0 {
        // Plot extends entirely into unloaded terrain — fall back to
        // a neutral mid-quality estimate. Replans after streaming
        // catches up will recompute via the carving idempotency path.
        return 0.5;
    }
    let avg_norm = ((sum as f32) / (count as f32) / 255.0).clamp(0.0, 1.0);
    let base = 0.25 + avg_norm * 0.75;
    if matches!(zone_kind, ZoneKind::Agricultural) {
        base * (0.5 + avg_norm * 0.5)
    } else {
        base
    }
}

/// Returns true if a faction may place a blueprint at this surface
/// tile under current tenure. **Phase 3** of the rollout: no transfers
/// have shipped yet, so every plot is `Tenure::StateOwned` of its
/// founding faction and the gate is effectively a no-op for same-
/// faction civic builds. Once Phase 4 ships household leases and the
/// first plot transfers, the same gate stops the chief from placing
/// civic structures on now-private land.
///
/// Rules:
/// - **Wild tile** (no plot indexed): always permitted. Most of the
///   world stays uncarved — only settlement zones become plots — and
///   wild gathering / outpost building must keep working.
/// - **State plot of `faction_id`**: permitted for any blueprint posted
///   by that faction. (Other factions are excluded — you can't build on
///   a rival state's land.)
/// - **Household plot**: permitted only when `requesting_household`
///   matches the holder's household id. Civic blueprints (no requesting
///   household) are denied here so the chief stops placing on private
///   land.
///
/// `requesting_household` is `Some(household_id)` for personal /
/// household-posted blueprints (`Blueprint.personal_owner` resolves to
/// a `HouseholdMember`), `None` for chief-posted civic blueprints.
pub fn tile_buildable_by(
    plot_index: &PlotIndex,
    plot_q: &Query<&Plot>,
    tile: (i32, i32),
    faction_id: u32,
    requesting_household: Option<u32>,
) -> bool {
    let Some(pid) = plot_index.plot_at(tile.0, tile.1) else {
        return true;
    };
    let Some(&entity) = plot_index.by_id.get(&pid) else {
        return true;
    };
    let Ok(plot) = plot_q.get(entity) else {
        return true;
    };
    holder_permits_build(plot.holder, faction_id, requesting_household)
}

/// Pure tenure check, factored out so unit tests can exercise the
/// gate without spinning up a Bevy `App`. Public callers should prefer
/// `tile_buildable_by` (it handles the index lookup + missing-entity
/// fallback).
pub fn holder_permits_build(
    holder: TenureHolder,
    faction_id: u32,
    requesting_household: Option<u32>,
) -> bool {
    match holder {
        TenureHolder::State { faction_id: fid } => fid == faction_id,
        TenureHolder::Household { faction_id: hh_id } => requesting_household == Some(hh_id),
    }
}

/// Plot dimensions per zone kind. `None` means the entire zone is
/// treated as one civic plot — appropriate for Civic / Sacred / Market /
/// Defense zones, which never get subdivided to households.
fn plot_size_for(kind: ZoneKind) -> Option<(u16, u16)> {
    match kind {
        ZoneKind::Residential => Some((6, 6)),
        ZoneKind::Crafting => Some((4, 4)),
        ZoneKind::Storage => Some((4, 4)),
        ZoneKind::Agricultural => Some((16, 16)),
        ZoneKind::Civic | ZoneKind::Sacred | ZoneKind::Market | ZoneKind::Defense => None,
    }
}

/// Subdivide `rect` into roughly `(pw × ph)` sub-rects. Trailing remainder
/// shorter than `pw/2` (or `ph/2`) is folded into the previous slice so
/// we don't emit slivers along the zone edge.
fn subdivide(rect: TileRect, pw: u16, ph: u16) -> Vec<TileRect> {
    if rect.w == 0 || rect.h == 0 {
        return Vec::new();
    }
    let pw = pw.max(1) as i32;
    let ph = ph.max(1) as i32;
    let x_end = rect.x0 + rect.w as i32;
    let y_end = rect.y0 + rect.h as i32;

    let row_starts = slice_starts(rect.y0, y_end, ph);
    let col_starts = slice_starts(rect.x0, x_end, pw);
    let mut out = Vec::with_capacity(row_starts.len() * col_starts.len());
    for i in 0..row_starts.len() {
        let y0 = row_starts[i];
        let y1 = row_starts.get(i + 1).copied().unwrap_or(y_end);
        for j in 0..col_starts.len() {
            let x0 = col_starts[j];
            let x1 = col_starts.get(j + 1).copied().unwrap_or(x_end);
            let w = (x1 - x0) as u16;
            let h = (y1 - y0) as u16;
            if w == 0 || h == 0 {
                continue;
            }
            out.push(TileRect::new(x0, y0, w, h));
        }
    }
    out
}

/// Compute the start coordinates of each slice covering `[start, end)`
/// with target stride `stride`. The final slice absorbs any remainder
/// less than `stride / 2` so we don't emit slivers.
fn slice_starts(start: i32, end: i32, stride: i32) -> Vec<i32> {
    let mut starts = Vec::new();
    if end <= start {
        return starts;
    }
    let mut x = start;
    while x < end {
        starts.push(x);
        let next = x + stride;
        let remaining = end - next;
        if remaining > 0 && remaining < stride / 2 {
            // Absorb the small tail into this slice.
            break;
        }
        x = next;
    }
    starts
}

/// Carve plots out of every faction's current `SettlementPlan` zones.
/// Idempotent: a faction whose `culture_hash` matches the value last
/// carved is skipped. Re-plans tear down the faction's existing plots
/// and rebuild — Phase 1 simplicity; the diff path lands later.
///
/// Plots are valued at carve time (Phase 2) — `compute_plot_value`
/// reads each plot's centre / corner fertility from the world. Re-plans
/// recompute valuations naturally; live revaluation under demand
/// pressure lands with the listings system in Phase 4+.
pub fn carve_plots_system(
    mut commands: Commands,
    plans: Res<SettlementPlans>,
    settlement_map: Res<SettlementMap>,
    settlement_q: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    mut plot_index: ResMut<PlotIndex>,
    mut field_tiles: ResMut<crate::simulation::farm::FieldTileIndex>,
) {
    // Stage replan inputs first so we don't borrow `plans` while
    // mutating `plot_index`.
    struct CarveJob {
        fid: u32,
        sid: u32,
        new_hash: u64,
        zones: Vec<(ZoneKind, TileRect)>,
        faction_home: (i32, i32),
        market_tile: (i32, i32),
        spine: StreetSpine,
    }
    let mut work: Vec<CarveJob> = Vec::new();
    for (&fid, plan) in plans.0.iter() {
        if fid == SOLO {
            continue;
        }
        if plan.zones.is_empty() {
            continue;
        }
        if plot_index.by_faction_hash.get(&fid).copied() == Some(plan.culture_hash) {
            continue;
        }
        let Some(sid) = settlement_map.first_for_faction(fid) else {
            continue;
        };
        let Some(&sett_entity) = settlement_map.by_id.get(&sid) else {
            continue;
        };
        let Ok(sett) = settlement_q.get(sett_entity) else {
            continue;
        };
        let Some(faction) = registry.factions.get(&fid) else {
            continue;
        };
        let zones: Vec<(ZoneKind, TileRect)> =
            plan.zones.iter().map(|z| (z.kind, z.rect)).collect();
        work.push(CarveJob {
            fid,
            sid: sid.0,
            new_hash: plan.culture_hash,
            zones,
            faction_home: faction.home_tile,
            market_tile: sett.market_tile,
            spine: plan.spine.clone(),
        });
    }

    // Phase 1: surface-only plots. Underground plot variants come with
    // the tunneling-aware land model in a later phase.
    const PLOT_Z: i8 = 0;

    for CarveJob {
        fid,
        sid: sid_u32,
        new_hash,
        zones,
        faction_home,
        market_tile,
        spine,
    } in work
    {
        // Tear down stale plots tied to this settlement.
        let stale_ids: Vec<PlotId> = plot_index
            .by_settlement
            .remove(&sid_u32)
            .unwrap_or_default();
        if !stale_ids.is_empty() {
            for pid in &stale_ids {
                if let Some(entity) = plot_index.by_id.remove(pid) {
                    commands.entity(entity).despawn();
                }
            }
            let stale_set: AHashSet<PlotId> = stale_ids.into_iter().collect();
            // Drop ag-tile entries for the torn-down plots before retaining
            // `by_tile`, so `tile_is_farm_protected` doesn't keep guarding a
            // field that no longer exists. (Tiles already stamped `Cropland`
            // intentionally stay tilled-looking — see plan risk note.)
            let stale_ag: Vec<(i32, i32)> = plot_index
                .by_tile
                .iter()
                .filter(|(_, pid)| stale_set.contains(pid))
                .map(|(t, _)| *t)
                .collect();
            for t in stale_ag {
                plot_index.ag_tiles.remove(&t);
                // Drop per-tile field state too — abandoned plots release
                // their nutrient history. Tile kind (Cropland or natural soil)
                // stays as-is.
                field_tiles.remove(t);
            }
            plot_index.by_tile.retain(|_, pid| !stale_set.contains(pid));
        }

        // Carve fresh plots.
        let mut new_ids: Vec<PlotId> = Vec::new();
        for (kind, rect) in zones {
            let rects = match plot_size_for(kind) {
                Some((pw, ph)) => subdivide(rect, pw, ph),
                None => vec![rect],
            };
            for r in rects {
                let pid = plot_index.alloc_id();
                let base_value = compute_plot_value(
                    r,
                    kind,
                    faction_home,
                    market_tile,
                    &chunk_map,
                    &gen,
                    &globe,
                );
                let (frontage_edge, access_tile) = frontage_for_rect(r, &spine);
                let plot = Plot {
                    id: pid,
                    settlement_id: sid_u32,
                    faction_id: fid,
                    rect: r,
                    z: PLOT_Z,
                    zone_kind: kind,
                    tenure: Tenure::StateOwned,
                    holder: TenureHolder::State { faction_id: fid },
                    base_value,
                    last_valued_tick: 0,
                    missed_payments: 0,
                    frontage_edge,
                    access_tile,
                    parent_plot: None,
                    plowed_year: None,
                };
                let entity = commands.spawn(plot).id();
                plot_index.by_id.insert(pid, entity);
                new_ids.push(pid);
                let is_ag = kind == ZoneKind::Agricultural;
                for ty in r.y0..r.y0 + r.h as i32 {
                    for tx in r.x0..r.x0 + r.w as i32 {
                        plot_index.by_tile.insert((tx, ty), pid);
                        if !is_ag {
                            continue;
                        }
                        plot_index.ag_tiles.insert((tx, ty));
                        // Seasonal-farming jellyfish: the carve pass NO
                        // LONGER stamps Cropland. Founders pay for tilling
                        // in Spring 1 via `prepare_field_task_system`. Tile
                        // role is preserved for visual ordering by callers
                        // that consult `field_tile_role(...)`, but the
                        // ChunkMap stays untouched here.
                        let z = chunk_map.surface_z_at(tx, ty);
                        let cur = chunk_map.tile_at(tx, ty, z);
                        let _ = field_tile_role(new_hash, fid, (tx, ty), r);
                        field_tiles.ensure_entry((tx, ty), pid, cur.fertility);
                    }
                }
            }
        }
        plot_index.by_settlement.insert(sid_u32, new_ids);
        plot_index.by_faction_hash.insert(fid, new_hash);
    }
}

/// Phase 4: publish State-owned plots as `Listing`s when the owning
/// faction's `LandPolicy` permits. Runs every game-quarter-day so
/// listings refresh roughly four times per game-day; cheap walk over
/// the global plot map (a few dozen plots per settlement, capped at
/// `TARGET_LISTINGS_PER_FACTION` per faction).
///
/// Listing kind selection:
/// - `state_sells_land` ⇒ Sale (Market preset).
/// - `state_rents_land` ⇒ Lease (Mixed preset). When both flags are on
///   we publish a Lease *and* a Sale entry for the same plot so
///   households below the freehold price can still rent.
/// - Civic / Sacred / Defense / Market zones are state-retained — their
///   whole-zone plots are never listed.
///
/// Sharecropping listings stay deferred until Phase 6 wires the
/// harvest-time settlement; they remain a `ListingKind` variant so the
/// data-model evolves cleanly.
pub fn land_listing_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    plot_index: Res<PlotIndex>,
    plot_q: Query<&Plot>,
    mut listings: ResMut<LandListings>,
) {
    let cadence = (TICKS_PER_DAY as u64 / 4).max(1);
    if clock.tick % cadence != 0 {
        return;
    }

    // Snapshot existing listings so we don't double-list. Count is per-
    // (faction, zone_kind) and per-plot — a plot already publishing under
    // multiple kinds (Sale + Lease + Sharecrop) only consumes one slot.
    let mut listed: AHashSet<PlotId> = AHashSet::new();
    let mut count_by_faction_zone: AHashMap<(u32, ZoneKind), usize> = AHashMap::new();
    let mut counted_plots: AHashSet<PlotId> = AHashSet::new();
    for l in listings.for_sale.iter().chain(listings.for_lease.iter()) {
        listed.insert(l.plot_id);
        if counted_plots.insert(l.plot_id) {
            if let Some(&entity) = plot_index.by_id.get(&l.plot_id) {
                if let Ok(plot) = plot_q.get(entity) {
                    *count_by_faction_zone
                        .entry((plot.faction_id, plot.zone_kind))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    let now = clock.tick;
    for (&pid, &entity) in plot_index.by_id.iter() {
        if listed.contains(&pid) {
            continue;
        }
        let Ok(plot) = plot_q.get(entity) else {
            continue;
        };
        if !matches!(plot.tenure, Tenure::StateOwned) {
            continue;
        }
        // Civic plots stay state-held — public works land, not market.
        if matches!(
            plot.zone_kind,
            ZoneKind::Civic | ZoneKind::Sacred | ZoneKind::Defense | ZoneKind::Market
        ) {
            continue;
        }
        // Phase 6: child farm plots are bound to a parent residential lot;
        // never listed independently.
        if plot.parent_plot.is_some() {
            continue;
        }
        let Some(faction) = registry.factions.get(&plot.faction_id) else {
            continue;
        };
        let lp = faction.land_policy;
        if !lp.state_sells_land && !lp.state_rents_land && !lp.state_sharecrops {
            continue;
        }
        if plot.base_value <= 0.0 {
            continue;
        }
        // Per-(faction, zone_kind) cap, counted by *plot id* — a plot that
        // publishes Sale + Lease + Sharecrop still consumes one slot, not
        // three.
        let cap = listings_cap_for(plot.zone_kind);
        if cap == 0 {
            continue;
        }
        let key = (plot.faction_id, plot.zone_kind);
        let count = count_by_faction_zone.entry(key).or_insert(0);
        if *count >= cap {
            continue;
        }
        let mut published_this_plot = false;

        if lp.state_sells_land {
            listings.for_sale.push(Listing {
                plot_id: pid,
                asking: plot.base_value,
                kind: ListingKind::Sale,
                listed_tick: now,
                unsold_days: 0,
            });
            published_this_plot = true;
        }
        if lp.state_rents_land {
            let rent = (plot.base_value * lp.rent_yield_pct).max(MIN_MONTHLY_RENT);
            listings.for_lease.push(Listing {
                plot_id: pid,
                asking: rent,
                kind: ListingKind::Lease,
                listed_tick: now,
                unsold_days: 0,
            });
            published_this_plot = true;
        }
        // Phase 6: agricultural plots also get sharecrop listings —
        // tenant farmer model. `asking` here carries the *share* fraction
        // so consumers can read the contract terms without re-fetching
        // the policy. Zero upfront so a landless household can take the
        // contract on day one.
        if lp.state_sharecrops && matches!(plot.zone_kind, ZoneKind::Agricultural) {
            listings.for_lease.push(Listing {
                plot_id: pid,
                asking: lp.default_share_to_landlord,
                kind: ListingKind::Sharecrop,
                listed_tick: now,
                unsold_days: 0,
            });
            published_this_plot = true;
        }
        if published_this_plot {
            *count += 1;
        }
    }
}

/// Split a harvest yield between tenant household and landlord under a
/// `Tenure::Sharecropping` contract. `share_to_landlord` is rounded
/// *down* in the landlord's favour so a 1-unit harvest at 30 % share
/// stays with the tenant; sharecropping rounding has historically
/// favoured tenants on small yields.
pub fn split_sharecrop_yield(qty: u32, share_to_landlord: f32) -> (u32, u32) {
    let landlord = ((qty as f32) * share_to_landlord.clamp(0.0, 1.0)).floor() as u32;
    let landlord = landlord.min(qty);
    (qty.saturating_sub(landlord), landlord)
}

/// If `(tx, ty)` sits on a `Tenure::Sharecropping` plot, returns
/// `Some((tenant_qty, landlord_qty, landlord_faction_id))` so the
/// caller can route each share. Returns `None` for any other tenure
/// (or for tiles outside any plot) so the standard harvest path keeps
/// running.
pub fn lookup_sharecrop_split(
    plot_index: &PlotIndex,
    plot_q: &Query<&Plot>,
    tx: i32,
    ty: i32,
    qty: u32,
) -> Option<(u32, u32, u32)> {
    let pid = plot_index.plot_at(tx, ty)?;
    let &entity = plot_index.by_id.get(&pid)?;
    let plot = plot_q.get(entity).ok()?;
    let Tenure::Sharecropping {
        share_to_landlord, ..
    } = plot.tenure
    else {
        return None;
    };
    let (tenant, landlord) = split_sharecrop_yield(qty, share_to_landlord);
    if landlord == 0 {
        return None;
    }
    Some((tenant, landlord, plot.faction_id))
}

/// Phase 4: households with treasury + no land yet acquire a plot.
/// Runs once per game-day; one acquisition per household per tick.
/// Preference: outright Sale (Freehold) when affordable, otherwise the
/// cheapest affordable Lease.
///
/// Affordability bounds (`HOUSEHOLD_LEASE_AFFORDABILITY = 40 %`,
/// `HOUSEHOLD_BUY_AFFORDABILITY = 70 %`) keep households from
/// disastrously over-committing on the first available listing — a
/// crude proxy for credit constraints. Households at or below
/// `HOUSEHOLD_MIN_TREASURY_FOR_LEASE` skip this tick entirely.
pub fn household_land_acquisition_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    plot_index: Res<PlotIndex>,
    mut plot_q: Query<&mut Plot>,
    mut listings: ResMut<LandListings>,
    profession_q: Query<&crate::simulation::person::Profession>,
    storage_q: Query<&crate::simulation::faction::FactionStorageTile>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }

    // Snapshot which households already hold a plot of each zone kind.
    // Pre-farm-planner this was a single-plot cap; now per-zone so a
    // household with housing can still acquire cropland.
    let mut households_zones: AHashMap<u32, AHashSet<ZoneKind>> = AHashMap::new();
    for (_, &entity) in plot_index.by_id.iter() {
        if let Ok(plot) = plot_q.get(entity) {
            if let TenureHolder::Household { faction_id } = plot.holder {
                households_zones
                    .entry(faction_id)
                    .or_default()
                    .insert(plot.zone_kind);
            }
        }
    }

    // Snapshot which households already have a `FactionStorageTile`. A
    // farmer household acquiring an agricultural plot gets its own tile
    // spawned (so private harvest/withdrawal stays out of village storage)
    // — but only once per household.
    let mut households_with_storage: AHashSet<u32> = AHashSet::new();
    for st in storage_q.iter() {
        households_with_storage.insert(st.faction_id);
    }

    struct Cand {
        household_id: u32,
        treasury: f32,
        parent_id: u32,
        head_is_farmer: bool,
    }
    let mut candidates: Vec<Cand> = Vec::new();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        let Some(parent_id) = faction.parent_faction else {
            continue;
        };
        if faction.treasury < HOUSEHOLD_MIN_TREASURY_FOR_LEASE {
            continue;
        }
        let head_is_farmer = faction
            .household_head
            .and_then(|h| profession_q.get(h).ok())
            .map_or(false, |p| {
                matches!(*p, crate::simulation::person::Profession::Farmer)
            });
        candidates.push(Cand {
            household_id: fid,
            treasury: faction.treasury,
            parent_id,
            head_is_farmer,
        });
    }
    if candidates.is_empty() {
        return;
    }

    let now = clock.tick;
    let mut consumed: AHashSet<PlotId> = AHashSet::new();

    // Spawns a household-private FactionStorageTile the first time a
    // household lands an Agricultural plot, so private harvest/withdrawal
    // routes through it. Called from BOTH the direct-Agricultural
    // acquisition path AND the Residential→child-Ag claim path (the latter
    // used to be silently skipped, leaving the household with no storage
    // tile to plant from / deposit into).
    let ensure_household_storage = |commands: &mut Commands,
                                    plot_q: &Query<&mut Plot>,
                                    households_with_storage: &mut AHashSet<u32>,
                                    household_id: u32,
                                    source_pid: PlotId| {
        if households_with_storage.contains(&household_id) {
            return;
        }
        let mut storage_tile: Option<(i32, i32)> = None;
        for (&_other_pid, &ent) in plot_index.by_id.iter() {
            if let Ok(other) = plot_q.get(ent) {
                if matches!(other.holder, TenureHolder::Household { faction_id }
                        if faction_id == household_id)
                    && other.zone_kind == ZoneKind::Residential
                {
                    storage_tile = other.access_tile.or(Some(other.rect.center()));
                    break;
                }
            }
        }
        if storage_tile.is_none() {
            if let Some(&ent) = plot_index.by_id.get(&source_pid) {
                if let Ok(plot) = plot_q.get(ent) {
                    storage_tile = plot.access_tile.or(Some(plot.rect.center()));
                }
            }
        }
        if let Some((sx, sy)) = storage_tile {
            let world_pos = crate::world::terrain::tile_to_world(sx, sy);
            commands.spawn((
                crate::simulation::faction::FactionStorageTile {
                    faction_id: household_id,
                },
                Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
                GlobalTransform::default(),
                Visibility::Hidden,
                InheritedVisibility::default(),
            ));
            households_with_storage.insert(household_id);
        }
    };

    // Are there any Farmer-headed households at all? If not, fall back to any
    // household with treasury for agricultural acquisitions — bootstrapping a
    // fresh village whose adults haven't selected Farmer yet.
    let any_farmer_household = candidates.iter().any(|c| c.head_is_farmer);

    // Acquire one plot per zone kind per candidate per tick. Residential is
    // the priority track (housing first); Agricultural is the secondary track
    // for farmer households (or any household when no farmers exist yet).
    let target_zones = [ZoneKind::Residential, ZoneKind::Agricultural];

    for cand in &candidates {
        for &target_zone in target_zones.iter() {
            // Skip if this household already holds a plot of this zone kind.
            if households_zones
                .get(&cand.household_id)
                .map_or(false, |s| s.contains(&target_zone))
            {
                continue;
            }
            // Agricultural acquisition prefers Farmer households. If any
            // farmer households exist, non-farmers skip the agricultural
            // track; if none exist yet, allow any household to bootstrap.
            if matches!(target_zone, ZoneKind::Agricultural)
                && any_farmer_household
                && !cand.head_is_farmer
            {
                continue;
            }

            let max_lease = cand.treasury * HOUSEHOLD_LEASE_AFFORDABILITY;
            let max_buy = cand.treasury * HOUSEHOLD_BUY_AFFORDABILITY;

            let mut best_sale: Option<(PlotId, f32, u32)> = None;
            for l in listings.for_sale.iter() {
                if consumed.contains(&l.plot_id) {
                    continue;
                }
                if l.asking > max_buy {
                    continue;
                }
                let Some(&entity) = plot_index.by_id.get(&l.plot_id) else {
                    continue;
                };
                let Ok(plot) = plot_q.get(entity) else {
                    continue;
                };
                if plot.faction_id != cand.parent_id || plot.zone_kind != target_zone {
                    continue;
                }
                if best_sale.as_ref().map(|b| l.asking < b.1).unwrap_or(true) {
                    best_sale = Some((l.plot_id, l.asking, plot.faction_id));
                }
            }
            let mut best_lease: Option<(PlotId, f32, u32)> = None;
            let mut best_sharecrop: Option<(PlotId, f32, u32)> = None;
            for l in listings.for_lease.iter() {
                if consumed.contains(&l.plot_id) {
                    continue;
                }
                let Some(&entity) = plot_index.by_id.get(&l.plot_id) else {
                    continue;
                };
                let Ok(plot) = plot_q.get(entity) else {
                    continue;
                };
                if plot.faction_id != cand.parent_id || plot.zone_kind != target_zone {
                    continue;
                }
                match l.kind {
                    ListingKind::Lease => {
                        if l.asking > max_lease {
                            continue;
                        }
                        if best_lease.as_ref().map(|b| l.asking < b.1).unwrap_or(true) {
                            best_lease = Some((l.plot_id, l.asking, plot.faction_id));
                        }
                    }
                    ListingKind::Sharecrop => {
                        if best_sharecrop
                            .as_ref()
                            .map(|b| l.asking < b.1)
                            .unwrap_or(true)
                        {
                            best_sharecrop = Some((l.plot_id, l.asking, plot.faction_id));
                        }
                    }
                    ListingKind::Sale => {}
                }
            }

            let (kind, pid, asking, landlord) = match (best_sale, best_lease, best_sharecrop) {
                (Some((p, a, f)), _, _) => (ListingKind::Sale, p, a, f),
                (None, Some((p, a, f)), _) => (ListingKind::Lease, p, a, f),
                (None, None, Some((p, a, f))) => (ListingKind::Sharecrop, p, a, f),
                _ => continue,
            };

            // Sharecrop has no upfront cost — skip the currency transfer.
            // Sale / Lease move treasury household → landlord atomically.
            let actually_paid = match kind {
                ListingKind::Sharecrop => 0.0,
                ListingKind::Sale | ListingKind::Lease => {
                    let paid = if let Some(hh) = registry.factions.get_mut(&cand.household_id) {
                        let amount = asking.min(hh.treasury);
                        hh.treasury -= amount;
                        if hh.treasury < 0.0 {
                            hh.treasury = 0.0;
                        }
                        amount
                    } else {
                        0.0
                    };
                    if paid <= 0.0 {
                        continue;
                    }
                    if let Some(landlord_f) = registry.factions.get_mut(&landlord) {
                        landlord_f.treasury += paid;
                    } else {
                        if let Some(hh) = registry.factions.get_mut(&cand.household_id) {
                            hh.treasury += paid;
                        }
                        continue;
                    }
                    paid
                }
            };
            let _ = actually_paid;

            // Mutate plot tenure + holder.
            let lp = registry
                .factions
                .get(&landlord)
                .map(|f| f.land_policy)
                .unwrap_or_default();
            let mut parent_centre: Option<(i32, i32)> = None;
            let mut parent_is_residential = false;
            if let Some(&entity) = plot_index.by_id.get(&pid) {
                if let Ok(mut plot) = plot_q.get_mut(entity) {
                    let period_days = lp.default_lease_period_days.max(1);
                    let period_ticks = (period_days as u64) * (TICKS_PER_DAY as u64);
                    plot.holder = TenureHolder::Household {
                        faction_id: cand.household_id,
                    };
                    plot.tenure = match kind {
                        ListingKind::Sale => Tenure::Freehold,
                        ListingKind::Lease => Tenure::Leased {
                            rent_per_month: asking,
                            period_days,
                            paid_through_tick: now + period_ticks,
                        },
                        ListingKind::Sharecrop => Tenure::Sharecropping {
                            share_to_landlord: lp.default_share_to_landlord,
                            paid_through_tick: now + period_ticks,
                        },
                    };
                    plot.missed_payments = 0;
                    parent_centre = Some(plot.rect.center());
                    parent_is_residential = plot.zone_kind == ZoneKind::Residential;
                }
            }
            consumed.insert(pid);

            // Phase 6: Household-attached child farm plot. When a household
            // acquires a Residential plot, claim the nearest unowned
            // Agricultural plot of the same village within range, marking it
            // as a child. The child mirrors the parent's tenure semantics so
            // the household has integrated land tenure across home + yard.
            const CHILD_FARM_MAX_DIST: i32 = 12;
            if parent_is_residential {
                if let Some(centre) = parent_centre {
                    let mut best_child: Option<(PlotId, i32)> = None;
                    for (&cid, &cent) in plot_index.by_id.iter() {
                        if cid == pid || consumed.contains(&cid) {
                            continue;
                        }
                        let Ok(child_plot) = plot_q.get(cent) else {
                            continue;
                        };
                        if child_plot.zone_kind != ZoneKind::Agricultural {
                            continue;
                        }
                        if child_plot.faction_id != cand.parent_id {
                            continue;
                        }
                        if !matches!(child_plot.tenure, Tenure::StateOwned) {
                            continue;
                        }
                        if child_plot.parent_plot.is_some() {
                            continue;
                        }
                        let d = chebyshev_dist(child_plot.rect.center(), centre);
                        if d > CHILD_FARM_MAX_DIST {
                            continue;
                        }
                        if best_child.as_ref().map_or(true, |b| d < b.1) {
                            best_child = Some((cid, d));
                        }
                    }
                    if let Some((cid, _)) = best_child {
                        let mut bound = false;
                        if let Some(&cent) = plot_index.by_id.get(&cid) {
                            if let Ok(mut child_plot) = plot_q.get_mut(cent) {
                                // Mirror parent tenure (already set above).
                                let period_days = lp.default_lease_period_days.max(1);
                                let period_ticks = (period_days as u64) * (TICKS_PER_DAY as u64);
                                child_plot.holder = TenureHolder::Household {
                                    faction_id: cand.household_id,
                                };
                                child_plot.tenure = match kind {
                                    ListingKind::Sale => Tenure::Freehold,
                                    ListingKind::Lease => Tenure::Leased {
                                        rent_per_month: 0.0,
                                        period_days,
                                        paid_through_tick: now + period_ticks,
                                    },
                                    ListingKind::Sharecrop => Tenure::Sharecropping {
                                        share_to_landlord: lp.default_share_to_landlord,
                                        paid_through_tick: now + period_ticks,
                                    },
                                };
                                child_plot.parent_plot = Some(pid);
                                child_plot.missed_payments = 0;
                                consumed.insert(cid);
                                // Mark this household as holding an agricultural
                                // plot too, so the outer per-zone loop won't try
                                // to re-acquire one for the same household.
                                households_zones
                                    .entry(cand.household_id)
                                    .or_default()
                                    .insert(ZoneKind::Agricultural);
                                bound = true;
                            }
                        }
                        if bound {
                            // BUG fix: this Residential→child-Ag claim path
                            // marks the household as Ag-holding, which used to
                            // suppress the storage spawn (gated on the outer
                            // loop's `target_zone == Agricultural` iteration
                            // that would now be skipped). Spawn here so the
                            // household has a storage tile to plant from /
                            // deposit into for `FarmScope::Private`.
                            ensure_household_storage(
                                &mut commands,
                                &plot_q,
                                &mut households_with_storage,
                                cand.household_id,
                                cid,
                            );
                        }
                    }
                }
            }
            // Direct-Agricultural-acquisition storage spawn (the legacy
            // branch; the child-claim path above also spawns via the same
            // helper).
            if matches!(target_zone, ZoneKind::Agricultural) {
                ensure_household_storage(
                    &mut commands,
                    &plot_q,
                    &mut households_with_storage,
                    cand.household_id,
                    pid,
                );
            }
            // Mark the just-acquired zone as held so the next iteration of
            // `target_zones` skips it for this candidate.
            households_zones
                .entry(cand.household_id)
                .or_default()
                .insert(target_zone);
        }
    }

    if !consumed.is_empty() {
        listings.for_sale.retain(|l| !consumed.contains(&l.plot_id));
        listings
            .for_lease
            .retain(|l| !consumed.contains(&l.plot_id));
    }
}

/// Phase 5: collect rent on `Tenure::Leased` plots and evict
/// chronically-defaulting tenants. Cadence: every 30 game-days
/// (`TICKS_PER_DAY * 30`) — leases bill monthly and missed payments
/// accumulate over the same window.
///
/// Per-tick algorithm: for each Leased plot whose `paid_through_tick`
/// has expired, attempt the household→landlord faction-treasury
/// transfer. On success: advance `paid_through_tick` by one period and
/// reset `missed_payments`. On failure (insufficient household
/// treasury): increment `missed_payments`; once it reaches
/// `EVICTION_MISS_THRESHOLD` the plot reverts to `StateOwned` (held by
/// the original landlord) and the listing system republishes it on its
/// next cycle.
///
/// **Eviction caveat (Phase 5 minimal):** structures on the plot stay
/// in place; their `personal_owner` and any `Bed.owner` fields aren't
/// rewritten. The next phase (sharecropping / harvest hooks) handles
/// downstream cleanup once we know which fields actually carry a
/// household reference.
pub fn rent_collection_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    plot_index: Res<PlotIndex>,
    mut plot_q: Query<&mut Plot>,
    mut evicted: EventWriter<PlotEvictedEvent>,
) {
    let cadence = (TICKS_PER_DAY as u64).saturating_mul(30).max(1);
    if clock.tick % cadence != 0 {
        return;
    }
    let now = clock.tick;

    // Snapshot leased plots so we can mutate `Plot` without holding a
    // long borrow. Each entry: (entity, plot_id, household_id,
    // landlord_faction, rent, period_days, paid_through_tick,
    // missed_payments).
    struct LeaseRow {
        entity: bevy::prelude::Entity,
        landlord: u32,
        household: u32,
        rent: f32,
        period_days: u32,
        paid_through_tick: u64,
        missed_payments: u8,
    }
    let mut rows: Vec<LeaseRow> = Vec::new();
    for (_, &entity) in plot_index.by_id.iter() {
        let Ok(plot) = plot_q.get(entity) else {
            continue;
        };
        let TenureHolder::Household {
            faction_id: household,
        } = plot.holder
        else {
            continue;
        };
        let Tenure::Leased {
            rent_per_month,
            period_days,
            paid_through_tick,
        } = plot.tenure
        else {
            continue;
        };
        // Phase 6: child plots inherit the parent lease; rent is collected
        // through the parent only. Skip children to avoid double-billing.
        if plot.parent_plot.is_some() {
            continue;
        }
        if now < paid_through_tick {
            continue;
        }
        rows.push(LeaseRow {
            entity,
            landlord: plot.faction_id,
            household,
            rent: rent_per_month,
            period_days,
            paid_through_tick,
            missed_payments: plot.missed_payments,
        });
    }

    let period_ticks = |period_days: u32| (period_days.max(1) as u64) * (TICKS_PER_DAY as u64);

    for row in rows {
        // Attempt the rent transfer. Treasury floor at 0 — destitute
        // households don't go into debt; they accumulate misses.
        let avail = registry
            .factions
            .get(&row.household)
            .map(|f| f.treasury)
            .unwrap_or(0.0);
        let paid = if avail >= row.rent {
            if let Some(hh) = registry.factions.get_mut(&row.household) {
                hh.treasury -= row.rent;
                if hh.treasury < 0.0 {
                    hh.treasury = 0.0;
                }
            }
            if let Some(lord) = registry.factions.get_mut(&row.landlord) {
                lord.treasury += row.rent;
            } else {
                // Landlord vanished — refund tenant to keep invariant.
                if let Some(hh) = registry.factions.get_mut(&row.household) {
                    hh.treasury += row.rent;
                }
                continue;
            }
            true
        } else {
            false
        };

        let Ok(mut plot) = plot_q.get_mut(row.entity) else {
            continue;
        };
        if paid {
            plot.tenure = Tenure::Leased {
                rent_per_month: row.rent,
                period_days: row.period_days,
                paid_through_tick: row.paid_through_tick + period_ticks(row.period_days),
            };
            plot.missed_payments = 0;
        } else {
            let next_misses = row.missed_payments.saturating_add(1);
            if next_misses >= EVICTION_MISS_THRESHOLD {
                // Evict: revert tenure + holder, then emit
                // `PlotEvictedEvent`. The landlord's
                // `caps.land.eviction_policy` rides on the event so
                // `evicted_plot_cleanup_system` can decide between
                // LeaveStructures (no-op) / RevertToState (mark
                // state-owned — same behaviour today, no
                // personal_owner fields to clear yet) / Demolish
                // (despawn structures + drop refunds).
                let policy = registry
                    .factions
                    .get(&row.landlord)
                    .map(|f| f.caps.land.eviction_policy)
                    .unwrap_or(crate::simulation::archetype::EvictionPolicy::LeaveStructures);
                let plot_rect = plot.rect;
                let plot_z = plot.z;
                let plot_id = plot.id;
                plot.tenure = Tenure::StateOwned;
                plot.holder = TenureHolder::State {
                    faction_id: row.landlord,
                };
                plot.missed_payments = 0;
                evicted.send(PlotEvictedEvent {
                    plot_entity: row.entity,
                    plot_id,
                    plot_rect,
                    plot_z,
                    landlord_faction: row.landlord,
                    policy,
                });
            } else {
                // Bump miss counter; advance paid_through_tick so the
                // next cycle re-checks (rather than re-billing the
                // same overdue month every system fire).
                plot.tenure = Tenure::Leased {
                    rent_per_month: row.rent,
                    period_days: row.period_days,
                    paid_through_tick: row.paid_through_tick + period_ticks(row.period_days),
                };
                plot.missed_payments = next_misses;
            }
        }
    }
}

/// P7b: drains `PlotEvictedEvent`s and acts on each according to the
/// landlord's `EvictionPolicy`.
///
/// - `LeaveStructures` — today's behaviour (Phase 5 minimal). No
///   structure cleanup; the plot reverts to state-owned and the
///   listing system republishes it on its next cycle.
/// - `RevertToState` — same as `LeaveStructures` for now; reserved for
///   per-structure `personal_owner` clearing once those fields exist.
/// - `Demolish` — walks `StructureIndex` over every tile in
///   `plot_rect`. For each entity it finds, drops the
///   `Deployable::compute_refund_drop` payload (if any) as a
///   `GroundItem` at the entity's tile, then despawns the structure.
///   Bedroll/Tent/Yurt all carry `Deployable`; mudbrick walls do not
///   and stay despawned without drops (mirrors today's nomadic
///   teardown behaviour). Skips entities whose `Transform` we can't
///   read (defensive — shouldn't happen in steady state).
pub fn evicted_plot_cleanup_system(
    mut commands: Commands,
    mut events: EventReader<PlotEvictedEvent>,
    structure_index: Res<crate::simulation::construction::StructureIndex>,
    transform_q: Query<&Transform>,
    deployable_q: Query<&crate::simulation::pack_deploy::Deployable>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut item_q: Query<&'static mut crate::simulation::items::GroundItem>,
) {
    use crate::simulation::archetype::EvictionPolicy;
    for ev in events.read() {
        match ev.policy {
            EvictionPolicy::LeaveStructures | EvictionPolicy::RevertToState => continue,
            EvictionPolicy::Demolish => {}
        }
        let r = ev.plot_rect;
        let mut victims: Vec<Entity> = Vec::new();
        for tx in r.x0..r.x0.saturating_add(r.w as i32) {
            for ty in r.y0..r.y0.saturating_add(r.h as i32) {
                if let Some(&e) = structure_index.0.get(&(tx, ty)) {
                    victims.push(e);
                }
            }
        }
        for entity in victims {
            // Refund first (while the entity still exists).
            if let Ok(deployable) = deployable_q.get(entity) {
                if let Some((rid, qty)) = deployable.compute_refund_drop() {
                    if let Ok(transform) = transform_q.get(entity) {
                        let (tx, ty) =
                            crate::world::terrain::world_to_tile(transform.translation.truncate());
                        crate::simulation::items::spawn_or_merge_ground_item(
                            &mut commands,
                            &spatial,
                            &mut item_q,
                            tx,
                            ty,
                            rid,
                            qty,
                        );
                    }
                }
            }
            commands.entity(entity).despawn_recursive();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn farm_protected_for_ag_tiles_and_plants() {
        use crate::simulation::plants::PlantMap;
        let mut pi = PlotIndex::default();
        pi.ag_tiles.insert((10, 10));
        let mut pm = PlantMap::default();
        pm.0.insert((20, 20), Entity::from_raw(1));

        // In an Agricultural plot → protected.
        assert!(tile_is_farm_protected(&pi, &pm, (10, 10)));
        // Carries a planted crop → protected.
        assert!(tile_is_farm_protected(&pi, &pm, (20, 20)));
        // Neither → not protected (road carving may pave it).
        assert!(!tile_is_farm_protected(&pi, &pm, (0, 0)));
    }

    #[test]
    fn tile_edge_delta_cardinal_directions() {
        assert_eq!(TileEdge::North.delta(), (0, 1));
        assert_eq!(TileEdge::South.delta(), (0, -1));
        assert_eq!(TileEdge::East.delta(), (1, 0));
        assert_eq!(TileEdge::West.delta(), (-1, 0));
    }

    #[test]
    fn tile_edge_toward_picks_dominant_axis() {
        // Larger |dx| → East/West
        assert_eq!(TileEdge::toward((0, 0), (5, 1)), TileEdge::East);
        assert_eq!(TileEdge::toward((0, 0), (-5, 1)), TileEdge::West);
        // Larger |dy| → North/South
        assert_eq!(TileEdge::toward((0, 0), (1, 5)), TileEdge::North);
        assert_eq!(TileEdge::toward((0, 0), (1, -5)), TileEdge::South);
        // Tie prefers x-axis (East for dx >= 0).
        assert_eq!(TileEdge::toward((0, 0), (3, 3)), TileEdge::East);
    }

    #[test]
    fn subdivide_clean_grid() {
        let rect = TileRect::new(0, 0, 12, 12);
        let plots = subdivide(rect, 6, 6);
        assert_eq!(plots.len(), 4);
        assert!(plots.iter().all(|p| p.w == 6 && p.h == 6));
    }

    #[test]
    fn subdivide_absorbs_trailing_sliver() {
        // 14-wide / stride 6 → starts at [0, 6]; the remaining 2 absorbs
        // into the final slice (since 2 < 6/2 = 3).
        let rect = TileRect::new(0, 0, 14, 6);
        let plots = subdivide(rect, 6, 6);
        assert_eq!(plots.len(), 2);
        assert_eq!(plots[1].w, 8);
        assert_eq!(plots[1].h, 6);
    }

    #[test]
    fn subdivide_keeps_full_strides() {
        // 13-wide / stride 6 → starts at [0, 6, 12]; 1 absorbs into
        // last slice. Three slices? No — once remaining < stride/2 we
        // stop, so [0, 6] with last=7 wide.
        let rect = TileRect::new(0, 0, 13, 6);
        let plots = subdivide(rect, 6, 6);
        assert_eq!(plots.len(), 2);
        assert_eq!(plots[0].w, 6);
        assert_eq!(plots[1].w, 7);
    }

    #[test]
    fn plot_size_for_zone_kinds() {
        assert_eq!(plot_size_for(ZoneKind::Residential), Some((6, 6)));
        assert_eq!(plot_size_for(ZoneKind::Agricultural), Some((16, 16)));
        assert_eq!(plot_size_for(ZoneKind::Civic), None);
    }

    #[test]
    fn state_plot_permits_owning_faction_only() {
        let h = TenureHolder::State { faction_id: 7 };
        assert!(holder_permits_build(h, 7, None));
        assert!(holder_permits_build(h, 7, Some(99))); // household member of same faction
        assert!(!holder_permits_build(h, 8, None)); // rival faction blocked
    }

    #[test]
    fn household_plot_permits_only_holder() {
        let h = TenureHolder::Household { faction_id: 42 };
        assert!(holder_permits_build(h, 7, Some(42))); // matching household
        assert!(!holder_permits_build(h, 7, Some(43))); // different household
        assert!(!holder_permits_build(h, 7, None)); // chief civic blocked
    }

    #[test]
    fn sharecrop_split_thirty_percent() {
        // 10 unit yield at 30 % share → tenant 7, landlord 3.
        let (tenant, landlord) = split_sharecrop_yield(10, 0.30);
        assert_eq!(tenant, 7);
        assert_eq!(landlord, 3);
    }

    #[test]
    fn sharecrop_split_rounds_in_tenant_favor() {
        // 1 unit at 50 %: floor(0.5) = 0 → all to tenant. Sharecrop
        // historically protected tenants on small yields.
        let (tenant, landlord) = split_sharecrop_yield(1, 0.5);
        assert_eq!(tenant, 1);
        assert_eq!(landlord, 0);
    }

    #[test]
    fn sharecrop_split_clamps_share() {
        // Share > 1.0 clamps to 1.0 (landlord cap = entire harvest).
        let (tenant, landlord) = split_sharecrop_yield(10, 1.5);
        assert_eq!(tenant, 0);
        assert_eq!(landlord, 10);
    }

    #[test]
    fn sharecrop_split_zero_share_keeps_full_yield() {
        let (tenant, landlord) = split_sharecrop_yield(10, 0.0);
        assert_eq!(tenant, 10);
        assert_eq!(landlord, 0);
    }

    // ── Settlement realism: field tile-role variation ───────────────────

    #[test]
    fn field_tile_role_is_deterministic() {
        let rect = TileRect::new(0, 0, 16, 16);
        let a = field_tile_role(0xCAFE, 1, (8, 8), rect);
        let b = field_tile_role(0xCAFE, 1, (8, 8), rect);
        assert_eq!(a, b, "same input must produce same role");
    }

    #[test]
    fn field_tile_role_perimeter_biases_to_grass_edge() {
        // Sample the rectangle and check that GrassEdge concentrates on
        // the perimeter (cheb-1 distance to the rect edge = 0).
        let rect = TileRect::new(0, 0, 16, 16);
        let mut perim_grass = 0usize;
        let mut interior_grass = 0usize;
        let mut perim_total = 0usize;
        let mut interior_total = 0usize;
        for y in 0..16 {
            for x in 0..16 {
                let on_edge = x == 0 || x == 15 || y == 0 || y == 15;
                let role = field_tile_role(0xCAFE, 1, (x, y), rect);
                if on_edge {
                    perim_total += 1;
                    if role == FieldTileRole::GrassEdge {
                        perim_grass += 1;
                    }
                } else {
                    interior_total += 1;
                    if role == FieldTileRole::GrassEdge {
                        interior_grass += 1;
                    }
                }
            }
        }
        let perim_frac = perim_grass as f32 / perim_total as f32;
        let interior_frac = interior_grass as f32 / interior_total as f32;
        assert!(
            perim_frac > interior_frac,
            "perimeter GrassEdge fraction ({}) should exceed interior ({})",
            perim_frac,
            interior_frac
        );
    }
}
