//! Farm plot assignment + plot-scoped helpers (farm-planner §7).
//!
//! Communal villages (where Grain has `chief_allocates_labor == true`) match
//! Farmer entities to state-owned Agricultural plots one-to-one. The chief
//! posts plot-scoped Farm jobs against those assignments via
//! `chief_job_posting_system` reading `FarmPlotAssignments`.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::core_ids;
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::land::{Plot, PlotId, PlotIndex, Tenure};
use crate::simulation::person::Profession;
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::ZoneKind;
use crate::world::seasons::TICKS_PER_DAY;

/// One-to-one matching of farmer worker entities to state-owned Agricultural
/// plots. Maintained by `chief_farm_plot_assignment_system`. Read by
/// `chief_job_posting_system` (plot-scoped Farm postings) and
/// `job_claim_system` (assigned-farmer-only claim restriction).
#[derive(Resource, Default, Debug)]
pub struct FarmPlotAssignments {
    pub farmer_to_plot: AHashMap<Entity, PlotId>,
    pub plot_to_farmer: AHashMap<PlotId, Entity>,
}

impl FarmPlotAssignments {
    pub fn assigned_plot(&self, farmer: Entity) -> Option<PlotId> {
        self.farmer_to_plot.get(&farmer).copied()
    }

    pub fn assigned_farmer(&self, plot: PlotId) -> Option<Entity> {
        self.plot_to_farmer.get(&plot).copied()
    }

    pub fn assign(&mut self, farmer: Entity, plot: PlotId) {
        // Drop any prior pairing both sides — assignments are 1:1.
        if let Some(prev_plot) = self.farmer_to_plot.remove(&farmer) {
            self.plot_to_farmer.remove(&prev_plot);
        }
        if let Some(prev_farmer) = self.plot_to_farmer.remove(&plot) {
            self.farmer_to_plot.remove(&prev_farmer);
        }
        self.farmer_to_plot.insert(farmer, plot);
        self.plot_to_farmer.insert(plot, farmer);
    }

    pub fn release_farmer(&mut self, farmer: Entity) {
        if let Some(plot) = self.farmer_to_plot.remove(&farmer) {
            self.plot_to_farmer.remove(&plot);
        }
    }

    pub fn release_plot(&mut self, plot: PlotId) {
        if let Some(farmer) = self.plot_to_farmer.remove(&plot) {
            self.farmer_to_plot.remove(&farmer);
        }
    }
}

/// Daily Economy-set system. Per village faction whose grain policy still has
/// `chief_allocates_labor`, greedy-match `Profession::Farmer` workers to
/// state-owned Agricultural plots in the same settlement by distance from the
/// farmer's home tile. Releases stale assignments when plots vanish, factions
/// dissolve, or workers stop being Farmers.
pub fn chief_farm_plot_assignment_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    plot_index: Res<PlotIndex>,
    plot_q: Query<&Plot>,
    profession_q: Query<(Entity, &Profession, &FactionMember, &Transform)>,
    mut assignments: ResMut<FarmPlotAssignments>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }

    // Drop stale entries first (plot gone, farmer despawned/demoted, etc.).
    let stale_farmers: Vec<Entity> = assignments
        .farmer_to_plot
        .keys()
        .copied()
        .filter(|f| {
            profession_q
                .get(*f)
                .map_or(true, |(_, p, _, _)| !matches!(p, Profession::Farmer))
        })
        .collect();
    for f in stale_farmers {
        assignments.release_farmer(f);
    }
    let stale_plots: Vec<PlotId> = assignments
        .plot_to_farmer
        .keys()
        .copied()
        .filter(|pid| {
            let Some(&ent) = plot_index.by_id.get(pid) else {
                return true;
            };
            let Ok(plot) = plot_q.get(ent) else {
                return true;
            };
            // Drop if plot left state ownership (e.g. transferred to a
            // household via auction) or is no longer agricultural.
            !matches!(plot.tenure, Tenure::StateOwned) || plot.zone_kind != ZoneKind::Agricultural
        })
        .collect();
    for pid in stale_plots {
        assignments.release_plot(pid);
    }

    // Greedy-match per village faction.
    for (&fid, faction) in registry.factions.iter() {
        if faction.parent_faction.is_some() {
            continue; // households don't own communal plots
        }
        let chief_allocates = faction.policy_for(core_ids::grain()).chief_allocates_labor;
        if !chief_allocates {
            continue;
        }

        // Collect this village's state-owned, unassigned Agricultural plots.
        let mut free_plots: Vec<(PlotId, (i32, i32))> = Vec::new();
        for (&pid, &ent) in plot_index.by_id.iter() {
            let Ok(plot) = plot_q.get(ent) else { continue };
            if plot.faction_id != fid
                || plot.zone_kind != ZoneKind::Agricultural
                || !matches!(plot.tenure, Tenure::StateOwned)
            {
                continue;
            }
            if assignments.plot_to_farmer.contains_key(&pid) {
                continue;
            }
            free_plots.push((pid, plot.rect.center()));
        }
        if free_plots.is_empty() {
            continue;
        }

        // Collect this village's unassigned Farmer workers + their tile.
        let mut free_farmers: Vec<(Entity, (i32, i32))> = Vec::new();
        for (e, prof, fm, tr) in profession_q.iter() {
            if !matches!(prof, Profession::Farmer) || fm.faction_id != fid {
                continue;
            }
            if assignments.farmer_to_plot.contains_key(&e) {
                continue;
            }
            let tile = (
                (tr.translation.x / 16.0).floor() as i32,
                (tr.translation.y / 16.0).floor() as i32,
            );
            free_farmers.push((e, tile));
        }
        if free_farmers.is_empty() {
            continue;
        }

        // Greedy: for each farmer (in arbitrary order), assign nearest free plot.
        for (farmer, ftile) in free_farmers {
            let Some((idx, &(plot_pid, _))) = free_plots
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, pcenter))| chebyshev(ftile, *pcenter))
            else {
                break;
            };
            assignments.assign(farmer, plot_pid);
            free_plots.swap_remove(idx);
            if free_plots.is_empty() {
                break;
            }
        }
    }
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Game-start seeding (farm-planner §15). Runs once at `OnEnter(Playing)`
/// after `seed_starting_buildings_system`. For every settled, non-SOLO
/// village faction, ensure at least one 16×16 Agricultural plot exists at a
/// good nearby spot, set its tenure based on the economy preset, and pre-
/// seed the appropriate storage with grain seeds so the first farm cycle
/// can begin immediately.
///
/// Skipped for nomadic factions (no plots / no settlement). Skipped for
/// any faction that already has at least one Agricultural plot — runtime
/// carving will own ongoing supply.
pub fn seed_starting_farms_system(
    mut commands: Commands,
    options: Res<crate::game_state::GameStartOptions>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    mut registry: ResMut<crate::simulation::faction::FactionRegistry>,
    mut plot_index: ResMut<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
) {
    use crate::simulation::land::Plot;
    use crate::simulation::settlement::TileRect;

    if !options.seed_buildings {
        // Sandbox / minimal start — skip seeding farms.
        return;
    }
    const PLOT_SIZE: i32 = 16;
    const PLOT_Z: i8 = 0;
    const STARTING_GRAIN_SEEDS: u32 = 32;

    // Snapshot which factions already have an Agricultural plot.
    let mut already_seeded: ahash::AHashSet<u32> = ahash::AHashSet::new();
    for (_, &ent) in plot_index.by_id.iter() {
        if let Ok(plot) = plot_q.get(ent) {
            if plot.zone_kind == ZoneKind::Agricultural {
                already_seeded.insert(plot.faction_id);
            }
        }
    }

    // Stage work — gather faction ids first so we don't borrow registry mutably while iterating.
    let mut work: Vec<(u32, crate::simulation::settlement::SettlementId, (i32, i32))> = Vec::new();
    for (&fid, faction) in registry.factions.iter() {
        if fid == 0 {
            continue;
        }
        if faction.parent_faction.is_some() {
            continue; // households don't seed plots — village does
        }
        if matches!(
            faction.lifestyle,
            crate::simulation::faction::Lifestyle::Nomadic
        ) {
            continue;
        }
        if already_seeded.contains(&fid) {
            continue;
        }
        let Some(sid) = settlement_map.first_for_faction(fid) else {
            continue;
        };
        work.push((fid, sid, faction.home_tile));
    }

    for (fid, sid, home) in work {
        // Try a few candidate offsets near home_tile for a passable Loam/
        // Silt/Grass patch. Prefer farmable terrain. If all fail, just
        // place at offset (8, 8) and let the carve helpers cope.
        let candidates: [(i32, i32); 8] = [
            (8, 0),
            (-24, 0),
            (0, 8),
            (0, -24),
            (8, 8),
            (-24, 8),
            (8, -24),
            (-24, -24),
        ];
        let (ox, oy) = candidates
            .iter()
            .copied()
            .find(|(dx, dy)| {
                // Sample center tile of candidate plot.
                let cx = home.0 + dx + PLOT_SIZE / 2;
                let cy = home.1 + dy + PLOT_SIZE / 2;
                let kind = chunk_map.tile_kind_at(cx, cy);
                matches!(
                    kind,
                    Some(crate::world::tile::TileKind::Grass)
                        | Some(crate::world::tile::TileKind::Loam)
                        | Some(crate::world::tile::TileKind::Silt)
                )
            })
            .unwrap_or((8, 0));
        let rect = TileRect::new(home.0 + ox, home.1 + oy, PLOT_SIZE as u16, PLOT_SIZE as u16);

        let pid = plot_index.alloc_id();
        let plot = Plot {
            id: pid,
            settlement_id: sid.0,
            faction_id: fid,
            rect,
            z: PLOT_Z,
            zone_kind: ZoneKind::Agricultural,
            tenure: Tenure::StateOwned,
            holder: crate::simulation::land::TenureHolder::State { faction_id: fid },
            base_value: crate::simulation::land::PLOT_BASE_VALUE,
            last_valued_tick: 0,
            missed_payments: 0,
            frontage_edge: None,
            access_tile: None,
            parent_plot: None,
        };
        let entity = commands.spawn(plot).id();
        plot_index.by_id.insert(pid, entity);
        plot_index.by_settlement.entry(sid.0).or_default().push(pid);
        for ty in rect.y0..rect.y0 + rect.h as i32 {
            for tx in rect.x0..rect.x0 + rect.w as i32 {
                plot_index.by_tile.insert((tx, ty), pid);
            }
        }

        // Pre-seed grain seeds into faction storage so the first chief
        // Farm posting (or private FarmWorkScorer) can dispatch on tick 1.
        if let Some(faction) = registry.factions.get_mut(&fid) {
            *faction
                .storage
                .totals
                .entry(crate::economy::core_ids::grain_seed())
                .or_insert(0) += STARTING_GRAIN_SEEDS;
        }
    }
}
