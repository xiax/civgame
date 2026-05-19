//! Mega-chunks and `SettledRegions` — the unit of player settlement and
//! world-map switching.
//!
//! A mega-chunk is `MEGACHUNK_SIZE_CHUNKS² = 16×16 = 256` chunks, i.e. a
//! 512×512 tile region. It's a coarse partition of the global tile grid that
//! the player can settle in. Mega-chunks have no biome of their own — biomes
//! are derived per-tile from the climate field — so a single mega-chunk can
//! contain mixed biomes and a biome can span many mega-chunks.

use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::globe::{Globe, MEGACHUNK_SIZE_CHUNKS};
use crate::world::terrain::{climate_fertility_estimate_at, TILE_SIZE};
use crate::world::tile::TileKind;
use ahash::AHashMap;
use bevy::prelude::*;

/// One focus point for chunk streaming. Chunks within `chunk_radius` of
/// `world_pos` are loaded; chunks within camera-tagged focus get sprites
/// spawned, others get only chunk data + sim.
#[derive(Clone, Copy, Debug)]
pub struct FocusPoint {
    pub world_pos: Vec2,
    pub chunk_radius: i32,
    /// True for the camera focus (the only one that gets sprites). False for
    /// off-camera settled-region centres (data + sim only).
    pub is_camera: bool,
}

#[derive(Resource, Default, Debug)]
pub struct SimulationFocus {
    pub points: Vec<FocusPoint>,
}

impl SimulationFocus {
    /// Test whether any focus point covers the given chunk coord within
    /// `extra` extra chunks. `extra=0` matches the focus's own radius;
    /// `extra=4` matches the unload radius (LOAD_RADIUS + 4).
    pub fn covers(&self, cx: i32, cy: i32, extra: i32) -> bool {
        for p in &self.points {
            let pcx = (p.world_pos.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let pcy = (p.world_pos.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let dx = (cx - pcx).abs();
            let dy = (cy - pcy).abs();
            if dx.max(dy) <= p.chunk_radius + extra {
                return true;
            }
        }
        false
    }

    /// Distance (in chunks) to the nearest camera-flagged focus, if any.
    pub fn distance_to_camera(&self, cx: i32, cy: i32) -> Option<i32> {
        let mut best: Option<i32> = None;
        for p in &self.points {
            if !p.is_camera {
                continue;
            }
            let pcx = (p.world_pos.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let pcy = (p.world_pos.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let dx = (cx - pcx).abs();
            let dy = (cy - pcy).abs();
            let d = dx.max(dy);
            best = Some(best.map(|b| b.min(d)).unwrap_or(d));
        }
        best
    }
}

pub type RegionId = u32;

/// Tile-width of one mega-chunk side.
pub const MEGACHUNK_TILES: i32 = MEGACHUNK_SIZE_CHUNKS * CHUNK_SIZE as i32;

/// Helpers for converting between mega-chunk, chunk, and tile coordinates.
pub struct MegaChunkCoord;

impl MegaChunkCoord {
    /// Mega-chunk that contains the given chunk coord.
    pub fn from_chunk(cx: i32, cy: i32) -> (i32, i32) {
        (
            cx.div_euclid(MEGACHUNK_SIZE_CHUNKS),
            cy.div_euclid(MEGACHUNK_SIZE_CHUNKS),
        )
    }

    /// Mega-chunk that contains the given world-tile.
    pub fn from_tile(tx: i32, ty: i32) -> (i32, i32) {
        (
            tx.div_euclid(MEGACHUNK_TILES),
            ty.div_euclid(MEGACHUNK_TILES),
        )
    }

    /// World-tile coordinate of the centre tile of a mega-chunk.
    pub fn center_tile(mx: i32, my: i32) -> (i32, i32) {
        let half = MEGACHUNK_TILES / 2;
        (mx * MEGACHUNK_TILES + half, my * MEGACHUNK_TILES + half)
    }

    /// Chunk-coord range covered by a mega-chunk, as `(cx0, cy0, cx1, cy1)`
    /// (exclusive upper).
    pub fn chunk_range(mx: i32, my: i32) -> (i32, i32, i32, i32) {
        let cx0 = mx * MEGACHUNK_SIZE_CHUNKS;
        let cy0 = my * MEGACHUNK_SIZE_CHUNKS;
        (
            cx0,
            cy0,
            cx0 + MEGACHUNK_SIZE_CHUNKS,
            cy0 + MEGACHUNK_SIZE_CHUNKS,
        )
    }

    /// Tile-coord range covered by a mega-chunk, as `(tx0, ty0, tx1, ty1)`
    /// (exclusive upper).
    pub fn tile_bounds(mx: i32, my: i32) -> (i32, i32, i32, i32) {
        let tx0 = mx * MEGACHUNK_TILES;
        let ty0 = my * MEGACHUNK_TILES;
        (tx0, ty0, tx0 + MEGACHUNK_TILES, ty0 + MEGACHUNK_TILES)
    }

    /// True iff `(tx, ty)` falls inside mega-chunk `(mx, my)`.
    pub fn contains_tile(mx: i32, my: i32, tx: i32, ty: i32) -> bool {
        let (x0, y0, x1, y1) = Self::tile_bounds(mx, my);
        tx >= x0 && tx < x1 && ty >= y0 && ty < y1
    }
}

/// Stride (`N×N`) used when sampling expected fertility across a mega-chunk.
/// Cost dominates on river-distance lookups (`Globe::nearest_river_chebyshev`
/// is O(river-vertex-count)), so this is kept modest. 8×8 covers a 512×512
/// mega-chunk at 64-tile spacing — enough to register narrow riparian bands
/// without exploding the world-map overlay's compute budget.
pub const MEGACHUNK_FERTILITY_SAMPLES_PER_SIDE: i32 = 8;

/// Average climate-derived fertility across a mega-chunk on an
/// `MEGACHUNK_FERTILITY_SAMPLES_PER_SIDE²` grid. Mirrors the chunk-gen
/// fertility formula but uses pure-climate elevation (no per-tile Perlin),
/// so it returns the *expected* fertility chunks at this mega-chunk would
/// produce — usable on the world map without loading any chunks.
pub fn average_fertility_in_megachunk(globe: &Globe, mx: i32, my: i32) -> u8 {
    let (tx0, ty0, tx1, ty1) = MegaChunkCoord::tile_bounds(mx, my);
    let n = MEGACHUNK_FERTILITY_SAMPLES_PER_SIDE;
    let dx = (tx1 - tx0) / n;
    let dy = (ty1 - ty0) / n;
    let mut sum: u32 = 0;
    let mut count: u32 = 0;
    for j in 0..n {
        for i in 0..n {
            let tx = tx0 + dx * i + dx / 2;
            let ty = ty0 + dy * j + dy / 2;
            sum += climate_fertility_estimate_at(globe, tx, ty) as u32;
            count += 1;
        }
    }
    if count == 0 {
        0
    } else {
        (sum / count).min(255) as u8
    }
}

#[derive(Clone, Debug)]
pub struct SettledRegion {
    pub megachunk: (i32, i32),
    pub founding_tick: u64,
    pub name: String,
    /// Last camera-world-position the player viewed in this region.
    pub camera_bookmark: Vec2,
    pub player_owned: bool,
}

#[derive(Resource, Default)]
pub struct SettledRegions {
    pub by_id: AHashMap<RegionId, SettledRegion>,
    /// Reverse lookup: mega-chunk → region id.
    pub by_megachunk: AHashMap<(i32, i32), RegionId>,
    pub next_id: RegionId,
}

impl SettledRegions {
    pub fn settle(
        &mut self,
        megachunk: (i32, i32),
        founding_tick: u64,
        name: String,
        camera_bookmark: Vec2,
        player_owned: bool,
    ) -> RegionId {
        if let Some(&id) = self.by_megachunk.get(&megachunk) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.by_megachunk.insert(megachunk, id);
        self.by_id.insert(
            id,
            SettledRegion {
                megachunk,
                founding_tick,
                name,
                camera_bookmark,
                player_owned,
            },
        );
        id
    }

    pub fn is_settled(&self, megachunk: (i32, i32)) -> bool {
        self.by_megachunk.contains_key(&megachunk)
    }
}

/// Outcome of `pick_player_home_in_megachunk` — useful for spawn-select
/// diagnostics (preview marker) and for `warn!` triage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeFallback {
    /// Best-scoring candidate from the random best-of-N sweep.
    BestOfN,
    /// Sweep failed; first passable tile from a centre-out chebyshev spiral.
    SpiralFromCenter,
    /// Even the spiral failed; centre tile was forced (warning logged).
    ForcedCenter,
}

#[derive(Clone, Copy, Debug)]
pub struct HomePick {
    pub tile: (i32, i32),
    pub score: i32,
    pub river_distance: u8,
    pub fallback: HomeFallback,
}

const HOME_PICK_BEST_OF_N: u32 = 200;
const HOME_PICK_MIN_SCORE: i32 = -49;

/// Deterministically pick the player faction's home tile inside the selected
/// mega-chunk. Same `(world_seed, mx, my)` → same `HomePick`, so the
/// spawn-select preview marker matches what `spawn_population` later spawns.
///
/// Algorithm: best-of-`HOME_PICK_BEST_OF_N` random samples inside
/// `MegaChunkCoord::tile_bounds(mx, my)`, rejecting impassable / stone / river
/// / water tiles. Scoring rewards river proximity (existing curve) and gives a
/// soft pull toward the cell centre. Fallbacks: centre-out chebyshev spiral,
/// then a forced centre with a warning.
pub fn pick_player_home_in_megachunk(
    chunk_map: &ChunkMap,
    mx: i32,
    my: i32,
    world_seed: u64,
) -> HomePick {
    let (tx0, ty0, tx1, ty1) = MegaChunkCoord::tile_bounds(mx, my);
    let (cx, cy) = MegaChunkCoord::center_tile(mx, my);
    let width = (tx1 - tx0) as u32;
    let height = (ty1 - ty0) as u32;

    let seed = home_pick_seed(world_seed, mx, my);
    let mut rng = fastrand::Rng::with_seed(seed);

    let tile_ok = |tx: i32, ty: i32| -> bool {
        if !chunk_map.is_passable(tx, ty) {
            return false;
        }
        match chunk_map.tile_kind_at(tx, ty) {
            Some(TileKind::Stone) | Some(TileKind::River) | Some(TileKind::Water) => false,
            _ => true,
        }
    };

    let score_tile = |tx: i32, ty: i32| -> i32 {
        let river_d = chunk_map.river_distance_at(tx, ty);
        // Match the AI faction picker: settlements need to fit on one bank
        // since bridges aren't available until Chalcolithic. Nomadic starts
        // get the same rule — they still drop seeded furniture + the full
        // member group at the home tile.
        let river_score = match river_d {
            0..=4 => -80,
            5..=9 => -20,
            10..=12 => 20,
            13..=16 => 60,
            _ => 0,
        };
        // Soft centre-pull (weight 10): edge-of-cell → 0, dead centre → +10.
        let half = MEGACHUNK_TILES / 2;
        let dnorm = ((tx - cx).abs().max((ty - cy).abs())) as f32 / half as f32;
        let dnorm = dnorm.min(1.0);
        let center_score = ((1.0 - dnorm) * 10.0) as i32;
        50 + river_score + center_score
    };

    let mut best: Option<HomePick> = None;
    for _ in 0..HOME_PICK_BEST_OF_N {
        let tx = tx0 + rng.u32(0..width) as i32;
        let ty = ty0 + rng.u32(0..height) as i32;
        if !tile_ok(tx, ty) {
            continue;
        }
        let score = score_tile(tx, ty);
        if best.as_ref().map_or(true, |p| score > p.score) {
            best = Some(HomePick {
                tile: (tx, ty),
                score,
                river_distance: chunk_map.river_distance_at(tx, ty),
                fallback: HomeFallback::BestOfN,
            });
        }
    }

    if let Some(p) = best {
        if p.score >= HOME_PICK_MIN_SCORE {
            return p;
        }
    }

    // Fallback 1: centre-out chebyshev spiral. Deterministic, exhaustive
    // within the cell. Caps at the half-width so we never leak out.
    let half = MEGACHUNK_TILES / 2;
    for r in 0..=half {
        if let Some((tx, ty)) = spiral_first_passable(cx, cy, r, mx, my, tile_ok) {
            return HomePick {
                tile: (tx, ty),
                score: score_tile(tx, ty),
                river_distance: chunk_map.river_distance_at(tx, ty),
                fallback: HomeFallback::SpiralFromCenter,
            };
        }
    }

    // Fallback 2: cell is entirely uninhabitable (should not happen — UI
    // blocks ocean/mountain dominant cells). Force centre and warn.
    warn!(
        "pick_player_home_in_megachunk: no habitable tile in mega-chunk ({},{}); forcing centre tile ({},{})",
        mx, my, cx, cy
    );
    HomePick {
        tile: (cx, cy),
        score: 0,
        river_distance: chunk_map.river_distance_at(cx, cy),
        fallback: HomeFallback::ForcedCenter,
    }
}

fn home_pick_seed(world_seed: u64, mx: i32, my: i32) -> u64 {
    // Splitmix64-style deterministic mix (matches the codebase convention in
    // `world::hydrology` / `simulation::faction`). Must NOT use
    // `ahash::AHasher::default()`: ahash's default hasher is keyed from
    // per-process entropy/ASLR, so identical `(world_seed, mx, my)` inputs
    // hashed differently every run — which made this "deterministic" home
    // pick (and every settlement layout derived from the home tile)
    // non-reproducible across processes and flaked seed-geometry tests.
    const SALT: u64 = 0x6369_7667_616D_655F; // b"civgame_"
    let mut x = world_seed
        .wrapping_add(SALT)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (mx as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ (my as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x
}

/// Walk the chebyshev ring at `r` around `(cx, cy)`, returning the first tile
/// for which `tile_ok` holds. Skips tiles outside mega-chunk `(mx, my)`.
fn spiral_first_passable(
    cx: i32,
    cy: i32,
    r: i32,
    mx: i32,
    my: i32,
    tile_ok: impl Fn(i32, i32) -> bool,
) -> Option<(i32, i32)> {
    if r == 0 {
        return tile_ok(cx, cy).then_some((cx, cy));
    }
    // Top + bottom rows.
    for dx in -r..=r {
        for &dy in &[-r, r] {
            let (tx, ty) = (cx + dx, cy + dy);
            if MegaChunkCoord::contains_tile(mx, my, tx, ty) && tile_ok(tx, ty) {
                return Some((tx, ty));
            }
        }
    }
    // Left + right columns (skip corners already covered).
    for dy in (-r + 1)..=(r - 1) {
        for &dx in &[-r, r] {
            let (tx, ty) = (cx + dx, cy + dy);
            if MegaChunkCoord::contains_tile(mx, my, tx, ty) && tile_ok(tx, ty) {
                return Some((tx, ty));
            }
        }
    }
    None
}

/// When a player-faction agent's tile crosses into an unsettled mega-chunk,
/// settle it. The agent walks across organically — no teleport, no special
/// state. The new region appears in the world-map switcher next frame and
/// gets its own `SimulationFocus` point so its chunks stream around it.
pub fn detect_edge_crossing_system(
    mut settled: ResMut<SettledRegions>,
    clock: Res<crate::simulation::schedule::SimClock>,
    player_faction: Res<crate::simulation::faction::PlayerFaction>,
    mut log_events: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    persons: Query<
        (
            Entity,
            &Transform,
            &crate::simulation::faction::FactionMember,
        ),
        With<crate::simulation::person::Person>,
    >,
) {
    use crate::ui::activity_log::{ActivityEntryKind, ActivityLogEvent};
    use crate::world::terrain::TILE_SIZE;

    for (entity, transform, member) in &persons {
        if member.faction_id != player_faction.faction_id {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let mc = MegaChunkCoord::from_tile(tx, ty);
        if settled.is_settled(mc) {
            continue;
        }
        let next_idx = settled.by_id.len();
        let name = format!("Outpost {}", next_idx);
        settled.settle(
            mc,
            clock.tick,
            name.clone(),
            transform.translation.truncate(),
            true,
        );
        log_events.send(ActivityLogEvent {
            tick: clock.tick,
            actor: entity,
            faction_id: member.faction_id,
            kind: ActivityEntryKind::RegionSettled {
                megachunk: mc,
                region_name: name,
            },
        });
        info!("New region settled at mega-chunk {:?}", mc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, CHUNK_SIZE};
    use crate::world::tile::TileKind;

    fn flat_chunk(surface_z: i8, kind: TileKind) -> Chunk {
        let surface_z_arr = Box::new([[surface_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[8u8; CHUNK_SIZE]; CHUNK_SIZE]);
        Chunk::new(surface_z_arr, surface_kind, surface_fertility)
    }

    fn flat_map_for_megachunks(mxy_range: i32) -> ChunkMap {
        let mut map = ChunkMap::default();
        let r_chunks = mxy_range * MEGACHUNK_SIZE_CHUNKS;
        for cy in -r_chunks..r_chunks {
            for cx in -r_chunks..r_chunks {
                map.0
                    .insert(ChunkCoord(cx, cy), flat_chunk(0, TileKind::Grass));
            }
        }
        map
    }

    #[test]
    fn megachunk_coord_roundtrip() {
        let (mx, my) = MegaChunkCoord::from_tile(700, -50);
        assert_eq!((mx, my), (1, -1));
        let (cx, cy) = MegaChunkCoord::center_tile(mx, my);
        assert!(cx >= mx * MEGACHUNK_TILES && cx < (mx + 1) * MEGACHUNK_TILES);
        assert!(cy >= my * MEGACHUNK_TILES && cy < (my + 1) * MEGACHUNK_TILES);
    }

    #[test]
    fn tile_bounds_exclusive_max() {
        let (x0, y0, x1, y1) = MegaChunkCoord::tile_bounds(0, 0);
        assert_eq!((x0, y0), (0, 0));
        assert_eq!((x1, y1), (MEGACHUNK_TILES, MEGACHUNK_TILES));
        assert!(MegaChunkCoord::contains_tile(0, 0, 0, 0));
        assert!(MegaChunkCoord::contains_tile(
            0,
            0,
            MEGACHUNK_TILES - 1,
            MEGACHUNK_TILES - 1
        ));
        assert!(!MegaChunkCoord::contains_tile(0, 0, MEGACHUNK_TILES, 0));
        assert!(!MegaChunkCoord::contains_tile(0, 0, 0, MEGACHUNK_TILES));
    }

    #[test]
    fn tile_bounds_negative_megachunk() {
        let (x0, y0, x1, y1) = MegaChunkCoord::tile_bounds(-1, -1);
        assert_eq!((x0, y0), (-MEGACHUNK_TILES, -MEGACHUNK_TILES));
        assert_eq!((x1, y1), (0, 0));
        assert!(MegaChunkCoord::contains_tile(-1, -1, -1, -1));
        assert!(!MegaChunkCoord::contains_tile(-1, -1, 0, 0));
    }

    #[test]
    fn contains_tile_roundtrip_with_from_tile() {
        for (tx, ty) in [(0, 0), (300, 300), (-1, -1), (700, -50), (1024, 2048)] {
            let (mx, my) = MegaChunkCoord::from_tile(tx, ty);
            assert!(
                MegaChunkCoord::contains_tile(mx, my, tx, ty),
                "({},{}) → ({},{}) should contain self",
                tx,
                ty,
                mx,
                my
            );
        }
    }

    #[test]
    fn home_pick_stays_inside_megachunk() {
        let map = flat_map_for_megachunks(2);
        for &(mx, my) in &[(0, 0), (-1, 0), (1, -1)] {
            let pick = pick_player_home_in_megachunk(&map, mx, my, 1234);
            assert!(
                MegaChunkCoord::contains_tile(mx, my, pick.tile.0, pick.tile.1),
                "home {:?} not inside mega-chunk ({},{})",
                pick.tile,
                mx,
                my
            );
        }
    }

    #[test]
    fn home_pick_deterministic_for_same_inputs() {
        let map = flat_map_for_megachunks(2);
        let a = pick_player_home_in_megachunk(&map, 0, 0, 42);
        let b = pick_player_home_in_megachunk(&map, 0, 0, 42);
        assert_eq!(a.tile, b.tile);
        let c = pick_player_home_in_megachunk(&map, 0, 0, 43);
        // Different seed should usually move the home; not strictly required
        // but a regression check that the seed actually flows through.
        assert_ne!(a.tile, c.tile);
    }

    #[test]
    fn home_pick_seed_is_process_stable() {
        // Pins the exact `home_pick_seed` output. The previous
        // `ahash::AHasher::default()` implementation was deterministic
        // *within* a process (fixed per-process key) but differed *across*
        // runs, so `home_pick_deterministic_for_same_inputs` never caught
        // it. A hard-coded expected value fails the instant any
        // entropy-keyed hasher or constant change is reintroduced.
        assert_eq!(home_pick_seed(42, 0, 0), 0x80CE_B342_0616_B827);
        assert_eq!(
            home_pick_seed(42, 0, 0),
            home_pick_seed(42, 0, 0),
            "seed derivation must be pure"
        );
        assert_ne!(home_pick_seed(42, 0, 0), home_pick_seed(43, 0, 0));
        assert_ne!(home_pick_seed(42, 0, 0), home_pick_seed(42, 1, 0));
        assert_ne!(home_pick_seed(42, 0, 0), home_pick_seed(42, 0, 1));
    }

    #[test]
    fn settle_idempotent() {
        let mut sr = SettledRegions::default();
        let id1 = sr.settle((3, 4), 0, "Foo".into(), Vec2::ZERO, true);
        let id2 = sr.settle((3, 4), 5, "Bar".into(), Vec2::ZERO, false);
        assert_eq!(id1, id2);
        assert_eq!(sr.by_id.len(), 1);
    }
}
