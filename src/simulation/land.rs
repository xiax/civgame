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
use crate::world::globe::Globe;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::terrain::{tile_at_3d, WorldGen};

pub type PlotId = u32;

/// Which side of a `Plot.rect` faces a carved street. Phase 2 of the
/// Construction Overhaul: lot-driven placement (Phase 3) anchors footprints
/// at the frontage edge so structures face their road.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileEdge {
    North,
    East,
    South,
    West,
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
pub const TARGET_LISTINGS_PER_FACTION: usize = 8;
pub const HOUSEHOLD_MIN_TREASURY_FOR_LEASE: f32 = 5.0;
pub const HOUSEHOLD_LEASE_AFFORDABILITY: f32 = 0.40;
pub const HOUSEHOLD_BUY_AFFORDABILITY: f32 = 0.70;
pub const MIN_MONTHLY_RENT: f32 = 0.5;
/// Phase 5: number of consecutive missed rent cycles after which a
/// tenant is evicted. Two months matches the historical lease grace
/// window — gives a household one chance to recover before losing the
/// plot.
pub const EVICTION_MISS_THRESHOLD: u8 = 2;

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
        TenureHolder::Household { faction_id: hh_id } => {
            requesting_household == Some(hh_id)
        }
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
        ZoneKind::Agricultural => Some((10, 10)),
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
                };
                let entity = commands.spawn(plot).id();
                plot_index.by_id.insert(pid, entity);
                new_ids.push(pid);
                for ty in r.y0..r.y0 + r.h as i32 {
                    for tx in r.x0..r.x0 + r.w as i32 {
                        plot_index.by_tile.insert((tx, ty), pid);
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

    // Snapshot existing listings so we don't double-list.
    let mut listed: AHashSet<PlotId> = AHashSet::new();
    let mut count_by_faction: AHashMap<u32, usize> = AHashMap::new();
    for l in listings.for_sale.iter().chain(listings.for_lease.iter()) {
        listed.insert(l.plot_id);
        if let Some(&entity) = plot_index.by_id.get(&l.plot_id) {
            if let Ok(plot) = plot_q.get(entity) {
                *count_by_faction.entry(plot.faction_id).or_insert(0) += 1;
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
        let count = count_by_faction.entry(plot.faction_id).or_insert(0);
        if *count >= TARGET_LISTINGS_PER_FACTION {
            continue;
        }

        if lp.state_sells_land {
            listings.for_sale.push(Listing {
                plot_id: pid,
                asking: plot.base_value,
                kind: ListingKind::Sale,
                listed_tick: now,
                unsold_days: 0,
            });
            *count += 1;
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
            *count += 1;
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
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    plot_index: Res<PlotIndex>,
    mut plot_q: Query<&mut Plot>,
    mut listings: ResMut<LandListings>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }

    // Snapshot which households already own/lease a plot — Phase 4 caps
    // each household at one plot until further phases add land hunger
    // signals (more members, second-house ambition).
    let mut households_with_plot: AHashSet<u32> = AHashSet::new();
    for (_, &entity) in plot_index.by_id.iter() {
        if let Ok(plot) = plot_q.get(entity) {
            if let TenureHolder::Household { faction_id } = plot.holder {
                households_with_plot.insert(faction_id);
            }
        }
    }

    struct Cand {
        household_id: u32,
        treasury: f32,
        parent_id: u32,
    }
    let mut candidates: Vec<Cand> = Vec::new();
    for (&fid, faction) in registry.factions.iter() {
        if fid == SOLO {
            continue;
        }
        let Some(parent_id) = faction.parent_faction else {
            continue;
        };
        if households_with_plot.contains(&fid) {
            continue;
        }
        if faction.treasury < HOUSEHOLD_MIN_TREASURY_FOR_LEASE {
            continue;
        }
        candidates.push(Cand {
            household_id: fid,
            treasury: faction.treasury,
            parent_id,
        });
    }
    if candidates.is_empty() {
        return;
    }

    let now = clock.tick;
    let mut consumed: AHashSet<PlotId> = AHashSet::new();

    for cand in candidates {
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
            if plot.faction_id != cand.parent_id {
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
            if plot.faction_id != cand.parent_id {
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
                    // Sharecrop has no upfront cost — household just
                    // needs to be eligible. `asking` carries the share
                    // fraction; lower share = better deal for tenant.
                    if best_sharecrop.as_ref().map(|b| l.asking < b.1).unwrap_or(true) {
                        best_sharecrop = Some((l.plot_id, l.asking, plot.faction_id));
                    }
                }
                ListingKind::Sale => {} // Sale lives on for_sale.
            }
        }

        // Preference order: own outright (Sale) → rent (Lease) →
        // sharecrop (no upfront, but harvest-time tax). Households
        // would rather buy than rent, and rent than sharecrop.
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
                    if let Some(&cent) = plot_index.by_id.get(&cid) {
                        if let Ok(mut child_plot) = plot_q.get_mut(cent) {
                            // Mirror parent tenure (already set above).
                            let period_days = lp.default_lease_period_days.max(1);
                            let period_ticks =
                                (period_days as u64) * (TICKS_PER_DAY as u64);
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
                        }
                    }
                }
            }
        }
    }

    if !consumed.is_empty() {
        listings.for_sale.retain(|l| !consumed.contains(&l.plot_id));
        listings.for_lease.retain(|l| !consumed.contains(&l.plot_id));
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

    let period_ticks =
        |period_days: u32| (period_days.max(1) as u64) * (TICKS_PER_DAY as u64);

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
                // Evict: revert tenure + holder. Phase 5 keeps
                // structures in place; downstream component cleanup
                // (Bed.owner, HomeBed, planted-crop ownership) lands
                // alongside the harvest-time hooks in Phase 6.
                plot.tenure = Tenure::StateOwned;
                plot.holder = TenureHolder::State {
                    faction_id: row.landlord,
                };
                plot.missed_payments = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(plot_size_for(ZoneKind::Agricultural), Some((10, 10)));
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
}
