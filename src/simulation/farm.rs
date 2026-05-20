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
use crate::world::terrain::world_to_tile;
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

/// Iterator over `(plot_id, rect)` for every state-owned Agricultural plot
/// belonging to `faction_id`. Reused by `jobs.rs` (multi-open Farm posting
/// §6) and `faction.rs` (plot-demand-aware Farmer promotion §7) so both
/// surfaces speak the same definition of "field that needs a farmer".
/// "Has work" today = existence; the planting dispatcher's per-tile rect
/// search short-circuits when nothing's left to plant.
pub fn state_owned_ag_plots_for_faction(
    faction_id: u32,
    plot_index: &PlotIndex,
    plot_q: &Query<&Plot>,
) -> Vec<(PlotId, crate::simulation::settlement::TileRect)> {
    let mut out = Vec::new();
    for (&pid, &ent) in plot_index.by_id.iter() {
        let Ok(plot) = plot_q.get(ent) else { continue };
        if plot.faction_id != faction_id {
            continue;
        }
        if plot.zone_kind != ZoneKind::Agricultural {
            continue;
        }
        if !matches!(plot.tenure, Tenure::StateOwned) {
            continue;
        }
        out.push((pid, plot.rect));
    }
    out
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
    registry: Res<crate::simulation::faction::FactionRegistry>,
    mut plot_index: ResMut<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    mut chunk_map: ResMut<crate::world::chunk::ChunkMap>,
    storage_tiles: Query<(&crate::simulation::faction::FactionStorageTile, &Transform)>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut ground_items: Query<&mut crate::simulation::items::GroundItem>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
) {
    use crate::simulation::land::Plot;
    use crate::simulation::organic_settlement::ParcelShape;
    use crate::simulation::settlement::TileRect;

    if !options.seed_buildings {
        // Sandbox / minimal start — skip seeding farms.
        return;
    }
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
        // Site the starting plot on an Agricultural BELT parcel from the
        // settlement brain (populated by the OnEnter kickoff survey) — so the
        // tick-0 farm is already outside town, consistent with the runtime
        // belt. Pick the belt parcel nearest home (deterministic tiebreak by
        // origin). If the brain produced no belt parcel (e.g. no fertile land
        // around the settlement), skip the plot entirely — `carve_plots_system`
        // will carve belt plots once the layout settles; we still pre-seed the
        // grain below so planting works the moment a plot exists. NO near-home
        // fallback (that was the "farms all over the base" regression).
        let belt_rect: Option<TileRect> = brains.0.get(&sid).and_then(|brain| {
            brain
                .parcels
                .iter()
                .filter_map(|p| match (p.district_hint, &p.shape) {
                    (
                        Some(crate::simulation::organic_settlement::DistrictKind::Agricultural),
                        ParcelShape::Rect(r),
                    ) => Some(*r),
                    _ => None,
                })
                .min_by_key(|r| {
                    let cx = r.x0 + r.w as i32 / 2;
                    let cy = r.y0 + r.h as i32 / 2;
                    (
                        (cx - home.0).abs().max((cy - home.1).abs()),
                        r.x0,
                        r.y0,
                    )
                })
        });

        if let Some(rect) = belt_rect {
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
            // Settlement realism: route the seeded plot through the same
            // `field_tile_role` mosaic the runtime carve uses. Brain's
            // `culture_hash` doubles as the seed; `layout_hash` is the
            // closest analogue at seed time.
            let culture_seed = brains
                .0
                .get(&sid)
                .map(|b| b.layout_hash ^ ((fid as u64) << 32))
                .unwrap_or(fid as u64);
            for ty in rect.y0..rect.y0 + rect.h as i32 {
                for tx in rect.x0..rect.x0 + rect.w as i32 {
                    plot_index.by_tile.insert((tx, ty), pid);
                    plot_index.ag_tiles.insert((tx, ty));
                    let z = chunk_map.surface_z_at(tx, ty);
                    let cur = chunk_map.tile_at(tx, ty, z);
                    let role = crate::simulation::land::field_tile_role(
                        culture_seed,
                        fid,
                        (tx, ty),
                        rect,
                    );
                    let tillable = cur.kind == crate::world::tile::TileKind::Grass
                        || (cur.kind.is_soil_like()
                            && cur.kind != crate::world::tile::TileKind::Cropland);
                    match role {
                        crate::simulation::land::FieldTileRole::Cropland => {
                            if tillable {
                                chunk_map.set_tile(
                                    tx,
                                    ty,
                                    z,
                                    crate::world::tile::TileData {
                                        kind: crate::world::tile::TileKind::Cropland,
                                        elevation: cur.elevation,
                                        fertility: cur.fertility.max(180),
                                        flags: cur.flags,
                                        ore: cur.ore,
                                    },
                                );
                                tile_changed.send(
                                    crate::world::chunk_streaming::TileChangedEvent { tx, ty },
                                );
                            }
                        }
                        crate::simulation::land::FieldTileRole::CroplandLow => {
                            if tillable {
                                chunk_map.set_tile(
                                    tx,
                                    ty,
                                    z,
                                    crate::world::tile::TileData {
                                        kind: crate::world::tile::TileKind::Cropland,
                                        elevation: cur.elevation,
                                        fertility: cur.fertility.min(110).max(80),
                                        flags: cur.flags,
                                        ore: cur.ore,
                                    },
                                );
                                tile_changed.send(
                                    crate::world::chunk_streaming::TileChangedEvent { tx, ty },
                                );
                            }
                        }
                        crate::simulation::land::FieldTileRole::SoilFallow
                        | crate::simulation::land::FieldTileRole::GrassEdge => {
                            // Leave underlying terrain.
                        }
                    }
                }
            }
        }

        // Pre-seed physical grain seeds at a faction storage tile. `FactionStorage`
        // is only a rollup cache and is rebuilt every Economy tick from
        // `GroundItem`s, so writing it directly would vanish before posting.
        let storage_tile = storage_tiles
            .iter()
            .find_map(|(storage, transform)| {
                (storage.faction_id == fid).then_some(world_to_tile(transform.translation.truncate()))
            })
            .unwrap_or(home);
        crate::simulation::items::spawn_or_merge_ground_item(
            &mut commands,
            &spatial,
            &mut ground_items,
            storage_tile.0,
            storage_tile.1,
            crate::economy::core_ids::grain_seed(),
            STARTING_GRAIN_SEEDS,
        );
    }
}
