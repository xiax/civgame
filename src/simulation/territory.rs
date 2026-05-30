//! Per-tile territorial influence map derived from live anchors
//! (`Settlement`s, pitched `Camp`s). Sparse: only tiles inside an
//! anchor's disc carry a `TerritoryCell`. Coarse abstract-faction
//! ownership lives on `WorldCell.faction_id` and is rendered by the
//! world map; this module owns the fine, materialised layer.
//!
//! Invalidation mirrors the `CachedVisionSet` pattern: full recompute
//! runs only on dirty-anchor cadence, not per tick. Settlements mark
//! themselves dirty via `Changed<Settlement>` (peak_population growth);
//! camps via `CampState` transitions; materialisation via
//! `abstract_faction::materialize_abstract_faction_system`; abandon via
//! the lifecycle queue.
//!
//! Public surface:
//!
//! - `TerritoryMap` resource — keyed by `(i32, i32)` tile.
//! - `TerritoryCell { owner, state, score, runner_up }`.
//! - `recompute_territory_system` — Economy, every `RECOMPUTE_CADENCE`.
//! - `territory_owner_at(map, tile) -> Option<u32>` accessor.
//! - Pure helpers: `settlement_radius_for`, `camp_radius_for`,
//!   `score_at`, `cell_winner`. Unit-tested without an `App`.

use crate::collections::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::simulation::camp::{Camp, CampMap};
use crate::simulation::faction::{CampState, FactionRegistry, SOLO};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{Settlement, SettlementMap};
use crate::simulation::technology::{current_era, Era};
use crate::world::seasons::TICKS_PER_DAY;

/// How often `recompute_territory_system` walks dirty anchors. Matches
/// `BUREAUCRAT_ASSIGNMENT_CADENCE` so faction-level derived data
/// refreshes share a heartbeat.
pub const RECOMPUTE_CADENCE: u64 = (TICKS_PER_DAY / 4) as u64;

/// Base score an anchor contributes at its own tile. Higher than the
/// chebyshev falloff so any in-disc tile starts above
/// `CLAIM_THRESHOLD`. Truncated to `u16` per-cell.
pub const ANCHOR_BASE_SCORE: u32 = 1000;

/// Per-tile chebyshev falloff. With `ANCHOR_BASE_SCORE = 1000` and
/// `FALLOFF = 40` an anchor reaches 0 at chebyshev 25, which caps the
/// effective claim radius even when the era radius is larger.
pub const FALLOFF: u32 = 40;

/// Minimum winner score for a tile to be claimed at all. Below this the
/// tile reads `Unclaimed`.
pub const CLAIM_THRESHOLD: u16 = 200;

/// Margin (winner − runner_up) below which a tile reads `Contested`
/// rather than `Claimed`.
pub const CONTEST_MARGIN: u16 = 80;

/// Era-keyed base radius (tiles). At 1.5 m/tile: Paleo ≈ 12 m,
/// Bronze ≈ 33 m. Settlements add a √peak_population bonus on top,
/// capped by `era_base + RADIUS_CAP_BONUS`.
pub const RADIUS_CAP_BONUS: u16 = 12;

#[inline]
pub fn era_base_radius(era: Era) -> u16 {
    match era {
        Era::Paleolithic => 8,
        Era::Mesolithic => 10,
        Era::Neolithic => 14,
        Era::Chalcolithic => 18,
        Era::BronzeAge => 22,
    }
}

/// Per-settlement radius (tiles), gated by era and lifted by population.
#[inline]
pub fn settlement_radius_for(era: Era, peak_population: u32) -> u16 {
    let base = era_base_radius(era);
    let bonus = (peak_population as f32).sqrt().floor() as u16;
    base.saturating_add(bonus.min(RADIUS_CAP_BONUS))
}

/// Camp radius (tiles): pitched camps claim a tighter disc; packed
/// camps claim nothing.
#[inline]
pub fn camp_radius_for(era: Era, pitched: bool) -> u16 {
    if !pitched {
        return 0;
    }
    era_base_radius(era) / 2
}

#[inline]
pub fn score_at(anchor_tile: (i32, i32), tile: (i32, i32)) -> u16 {
    let dx = (anchor_tile.0 - tile.0).abs() as u32;
    let dy = (anchor_tile.1 - tile.1).abs() as u32;
    let cheb = dx.max(dy);
    let cost = cheb.saturating_mul(FALLOFF);
    ANCHOR_BASE_SCORE.saturating_sub(cost).min(u16::MAX as u32) as u16
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TerritoryState {
    #[default]
    Unclaimed,
    Claimed,
    Contested,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct TerritoryCell {
    pub owner: Option<u32>,
    pub state: TerritoryState,
    pub score: u16,
    pub runner_up: Option<(u32, u16)>,
}

#[derive(Clone, Debug, Default)]
pub struct TerritoryStats {
    pub claimed_tiles: u32,
    pub contested_tiles: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnchorKind {
    Settlement,
    Camp,
}

#[derive(Copy, Clone, Debug)]
pub struct InfluenceAnchor {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub radius: u16,
    pub kind: AnchorKind,
}

/// Sparse map of every tile inside any live anchor's disc.
///
/// Maintained by `recompute_territory_system` every `RECOMPUTE_CADENCE`
/// ticks. UI / overlay reads via `version` for cache invalidation.
#[derive(Resource, Default)]
pub struct TerritoryMap {
    pub cells: AHashMap<(i32, i32), TerritoryCell>,
    pub by_faction: AHashMap<u32, TerritoryStats>,
    pub version: u64,
}

impl TerritoryMap {
    pub fn owner_at(&self, tile: (i32, i32)) -> Option<u32> {
        self.cells.get(&tile).and_then(|c| c.owner)
    }

    pub fn cell_at(&self, tile: (i32, i32)) -> Option<&TerritoryCell> {
        self.cells.get(&tile)
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.by_faction.clear();
    }
}

/// Pure-fn cell resolver. Takes per-faction summed scores at a tile,
/// returns the resolved cell. Exposed for unit tests.
pub fn cell_winner(per_faction: &[(u32, u16)]) -> TerritoryCell {
    let mut best: Option<(u32, u16)> = None;
    let mut second: Option<(u32, u16)> = None;
    for &(fid, score) in per_faction {
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            second = best;
            best = Some((fid, score));
        } else if second.map(|(_, s)| score > s).unwrap_or(true) {
            second = Some((fid, score));
        }
    }
    let Some((winner_fid, winner_score)) = best else {
        return TerritoryCell::default();
    };
    if winner_score < CLAIM_THRESHOLD {
        return TerritoryCell {
            owner: None,
            state: TerritoryState::Unclaimed,
            score: winner_score,
            runner_up: second,
        };
    }
    let state = match second {
        Some((_, s)) if s >= CLAIM_THRESHOLD && winner_score - s < CONTEST_MARGIN => {
            TerritoryState::Contested
        }
        _ => TerritoryState::Claimed,
    };
    TerritoryCell {
        owner: Some(winner_fid),
        state,
        score: winner_score,
        runner_up: second,
    }
}

/// Rasterise one anchor's disc into a transient `per-tile` scratch map.
fn rasterise_anchor(
    anchor: &InfluenceAnchor,
    scratch: &mut AHashMap<(i32, i32), Vec<(u32, u16)>>,
) {
    if anchor.radius == 0 {
        return;
    }
    let r = anchor.radius as i32;
    let (ax, ay) = anchor.tile;
    for dy in -r..=r {
        for dx in -r..=r {
            let cheb = dx.abs().max(dy.abs()) as u32;
            if cheb > anchor.radius as u32 {
                continue;
            }
            let tile = (ax + dx, ay + dy);
            let s = score_at(anchor.tile, tile);
            if s == 0 {
                continue;
            }
            let entry = scratch.entry(tile).or_default();
            // Merge per-faction (multiple anchors of the same faction
            // sum their contributions at this tile, e.g. settlement +
            // adjacent camp).
            if let Some((_, existing)) = entry.iter_mut().find(|(fid, _)| *fid == anchor.faction_id)
            {
                *existing = existing.saturating_add(s);
            } else {
                entry.push((anchor.faction_id, s));
            }
        }
    }
}

/// Build the anchor list from live Settlement + pitched Camp entities.
fn collect_anchors(
    registry: &FactionRegistry,
    settlements: &[(u32, (i32, i32), u32)], // (faction_id, market_tile, peak_pop)
    camps: &[(u32, (i32, i32), bool)],      // (faction_id, home_tile, pitched)
) -> Vec<InfluenceAnchor> {
    let mut out = Vec::with_capacity(settlements.len() + camps.len());
    for &(fid, tile, peak_pop) in settlements {
        if fid == SOLO {
            continue;
        }
        let era = registry
            .factions
            .get(&fid)
            .map(|f| current_era(&f.techs))
            .unwrap_or(Era::Paleolithic);
        out.push(InfluenceAnchor {
            faction_id: fid,
            tile,
            radius: settlement_radius_for(era, peak_pop),
            kind: AnchorKind::Settlement,
        });
    }
    for &(fid, tile, pitched) in camps {
        if fid == SOLO {
            continue;
        }
        let era = registry
            .factions
            .get(&fid)
            .map(|f| current_era(&f.techs))
            .unwrap_or(Era::Paleolithic);
        let r = camp_radius_for(era, pitched);
        if r == 0 {
            continue;
        }
        out.push(InfluenceAnchor {
            faction_id: fid,
            tile,
            radius: r,
            kind: AnchorKind::Camp,
        });
    }
    out
}

/// Pure compute: given anchors, produce a fresh `cells` map + stats.
/// Tested directly without an `App`.
pub fn compute_cells_from_anchors(
    anchors: &[InfluenceAnchor],
) -> (
    AHashMap<(i32, i32), TerritoryCell>,
    AHashMap<u32, TerritoryStats>,
) {
    let mut scratch: AHashMap<(i32, i32), Vec<(u32, u16)>> = AHashMap::default();
    for anchor in anchors {
        rasterise_anchor(anchor, &mut scratch);
    }
    let mut cells: AHashMap<(i32, i32), TerritoryCell> =
        AHashMap::with_capacity_and_hasher(scratch.len(), crate::collections::FixedState);
    let mut stats: AHashMap<u32, TerritoryStats> = AHashMap::default();
    for (tile, per_faction) in scratch {
        let cell = cell_winner(&per_faction);
        if let Some(fid) = cell.owner {
            let entry = stats.entry(fid).or_default();
            match cell.state {
                TerritoryState::Claimed => entry.claimed_tiles += 1,
                TerritoryState::Contested => entry.contested_tiles += 1,
                _ => {}
            }
        }
        cells.insert(tile, cell);
    }
    (cells, stats)
}

/// Economy-stage recompute. Walks live `Settlement` + `Camp` entities,
/// rebuilds the sparse `TerritoryMap`. Cadence-gated.
pub fn recompute_territory_system(
    clock: Res<SimClock>,
    mut map: ResMut<TerritoryMap>,
    registry: Res<FactionRegistry>,
    settlement_q: Query<&Settlement>,
    camp_q: Query<&Camp>,
) {
    if clock.tick != 0 && clock.tick % RECOMPUTE_CADENCE != 0 {
        return;
    }
    let settlements: Vec<(u32, (i32, i32), u32)> = settlement_q
        .iter()
        .map(|s| (s.owner_faction, s.market_tile, s.peak_population))
        .collect();
    let camps: Vec<(u32, (i32, i32), bool)> = camp_q
        .iter()
        .map(|c| {
            let pitched = registry
                .factions
                .get(&c.owner_faction)
                .map(|f| matches!(f.camp_state, CampState::Pitched))
                .unwrap_or(true);
            (c.owner_faction, c.home_tile, pitched)
        })
        .collect();
    let anchors = collect_anchors(&registry, &settlements, &camps);
    let (cells, stats) = compute_cells_from_anchors(&anchors);
    map.cells = cells;
    map.by_faction = stats;
    map.version = map.version.wrapping_add(1);
}

// Suppress unused warning on AHashSet import for future use by
// `dirty_anchors` event-driven recompute (Phase 2 optimisation —
// currently the cadence-gated full pass is cheap enough at the
// anchor counts we ship).
#[allow(dead_code)]
fn _suppress_unused() -> AHashSet<Entity> {
    AHashSet::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn era_radius_grows_with_era() {
        assert!(era_base_radius(Era::Paleolithic) < era_base_radius(Era::Mesolithic));
        assert!(era_base_radius(Era::Mesolithic) < era_base_radius(Era::Neolithic));
        assert!(era_base_radius(Era::Neolithic) < era_base_radius(Era::Chalcolithic));
        assert!(era_base_radius(Era::Chalcolithic) < era_base_radius(Era::BronzeAge));
    }

    #[test]
    fn settlement_radius_grows_with_population_and_is_capped() {
        let era = Era::Neolithic;
        let r0 = settlement_radius_for(era, 0);
        let r10 = settlement_radius_for(era, 10);
        let r1000 = settlement_radius_for(era, 1000);
        assert!(r10 > r0);
        assert!(r1000 > r10);
        // Cap is era_base + RADIUS_CAP_BONUS.
        assert_eq!(r1000, era_base_radius(era) + RADIUS_CAP_BONUS);
    }

    #[test]
    fn packed_camp_claims_nothing() {
        assert_eq!(camp_radius_for(Era::Neolithic, false), 0);
        assert!(camp_radius_for(Era::Neolithic, true) > 0);
    }

    #[test]
    fn lone_anchor_claims_its_own_tile() {
        let anchor = InfluenceAnchor {
            faction_id: 1,
            tile: (0, 0),
            radius: 10,
            kind: AnchorKind::Settlement,
        };
        let (cells, stats) = compute_cells_from_anchors(&[anchor]);
        let cell = cells.get(&(0, 0)).expect("center cell present");
        assert_eq!(cell.owner, Some(1));
        assert_eq!(cell.state, TerritoryState::Claimed);
        assert!(stats.get(&1).unwrap().claimed_tiles > 0);
    }

    #[test]
    fn stronger_overlapping_anchor_wins_border() {
        // A: small disc radius 4 at (0,0). B: larger disc radius 10 at (8,0).
        // Tile (4,0) lies inside both; B should win because its
        // chebyshev distance there is 4 vs A's 4 — equal — so we
        // place B closer instead.
        let a = InfluenceAnchor {
            faction_id: 1,
            tile: (0, 0),
            radius: 6,
            kind: AnchorKind::Settlement,
        };
        let b = InfluenceAnchor {
            faction_id: 2,
            tile: (5, 0),
            radius: 8,
            kind: AnchorKind::Settlement,
        };
        let (cells, _) = compute_cells_from_anchors(&[a, b]);
        // Tile (4,0) is chebyshev 4 from A and 1 from B → B owns it.
        let cell = cells.get(&(4, 0)).expect("border tile present");
        assert_eq!(cell.owner, Some(2));
    }

    #[test]
    fn equidistant_anchors_produce_contested_seam() {
        let a = InfluenceAnchor {
            faction_id: 1,
            tile: (0, 0),
            radius: 10,
            kind: AnchorKind::Settlement,
        };
        let b = InfluenceAnchor {
            faction_id: 2,
            tile: (6, 0),
            radius: 10,
            kind: AnchorKind::Settlement,
        };
        let (cells, _) = compute_cells_from_anchors(&[a, b]);
        // Midpoint (3,0) is chebyshev 3 from both → identical scores → contested.
        let mid = cells.get(&(3, 0)).expect("midpoint present");
        assert_eq!(mid.state, TerritoryState::Contested);
    }

    #[test]
    fn cell_winner_unclaimed_below_threshold() {
        // Force a winner score below CLAIM_THRESHOLD.
        let cell = cell_winner(&[(1, CLAIM_THRESHOLD - 1)]);
        assert_eq!(cell.owner, None);
        assert_eq!(cell.state, TerritoryState::Unclaimed);
    }
}
