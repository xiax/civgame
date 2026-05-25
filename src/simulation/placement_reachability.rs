//! Shared placement-reachability layer.
//!
//! **Authoritative check — `path_exists`:** a bounded, agent-faithful A* over
//! the live [`ChunkMap`] using the *same* canonical step rules the real
//! pathfinder uses (`ChunkMap::passable_step_3d` /
//! `pathfinding::step::passable_diagonal_step`). A found path proves
//! walkability; a tile sealed off *within* a chunk is correctly rejected
//! because the search physically cannot cross the wall. Correct at seed time
//! (reads current tiles, including walls `seed_walled_house_at` stamps during
//! the seed pass) and at runtime.
//!
//! **Optional `connectivity_prefilter`:** an O(1) fast-reject over the *settled*
//! [`ChunkConnectivity`] graph. Only valid at runtime (the graph is not
//! reliably built during `OnEnter(Playing)` and would not reflect seeded
//! walls). Never the sole authority — always confirm a `Some(true)` / `None`
//! with `path_exists`.
//!
//! Every placement surface (houses, starting farms, kitchen gardens,
//! `spawn_population`, market households, organic site choice, chief directives,
//! farm-tile dispatch) routes through this one module so "we said reachable"
//! can never drift from what the agent pathfinder will actually do. The legacy
//! `construction::doormat_reaches_home` BFS is folded in here — no parallel
//! reachability implementation survives.

use ahash::{AHashMap, AHashSet};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::step::passable_diagonal_step;
use crate::world::chunk::{ChunkMap, Z_MAX, Z_MIN};

/// One-shot seed-time checks run once per faction at startup and can afford a
/// generous bound (covers a ~50-tile open radius).
pub const SEED_MAX_EXPANSIONS: usize = 12_000;
/// Runtime checks are short-range (within a settlement / a 16×16 plot).
pub const RUNTIME_MAX_EXPANSIONS: usize = 4_000;
/// Matches the legacy `MAX_DOORMAT_BFS_STEPS` budget for the door-cardinal path.
pub const DOORMAT_MAX_EXPANSIONS: usize = 1_500;

const DIRS8: [(i32, i32); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

#[inline]
fn cheby(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Tunable bounds + optional impassability overlay for [`path_exists`].
pub struct ReachOpts<'a> {
    /// A* expansion ceiling. The search fails closed (returns `false`) if it
    /// can't prove a path within this budget.
    pub max_expansions: usize,
    /// Extra impassability: any tile column `(x, y, _)` for which this returns
    /// true is treated as blocked *in addition to* terrain. Models planned
    /// walls before they are stamped (simulated house) or the door-cardinal
    /// Stone-aversion heuristic.
    pub blocked: Option<&'a dyn Fn((i32, i32, i32)) -> bool>,
}

impl<'a> ReachOpts<'a> {
    pub fn seed() -> Self {
        Self {
            max_expansions: SEED_MAX_EXPANSIONS,
            blocked: None,
        }
    }
    pub fn runtime() -> Self {
        Self {
            max_expansions: RUNTIME_MAX_EXPANSIONS,
            blocked: None,
        }
    }
    pub fn with_blocked(mut self, f: &'a dyn Fn((i32, i32, i32)) -> bool) -> Self {
        self.blocked = Some(f);
        self
    }
    pub fn with_cap(mut self, cap: usize) -> Self {
        self.max_expansions = cap;
        self
    }
}

/// Resolve a 2D tile to a standable 3D node using the surface as the hint —
/// the same z-resolution agents use when a caller only knows `(x, y)`.
#[inline]
pub fn resolve3(chunk_map: &ChunkMap, tile: (i32, i32)) -> (i32, i32, i32) {
    let sz = chunk_map.surface_z_at(tile.0, tile.1);
    let z = chunk_map.nearest_standable_z(tile.0, tile.1, sz);
    (tile.0, tile.1, z)
}

/// Authoritative bounded A* path-existence check. Returns true iff a concrete
/// agent-walkable path from `from` to the goal *column* `(to.0, to.1)` exists
/// within `opts.max_expansions`. See module docs.
pub fn path_exists(
    chunk_map: &ChunkMap,
    from: (i32, i32, i32),
    to: (i32, i32, i32),
    opts: ReachOpts,
) -> bool {
    if (from.0, from.1) == (to.0, to.1) {
        return true;
    }
    let blocked = |t: (i32, i32, i32)| opts.blocked.map_or(false, |f| f(t));
    // Goal must be a standable, non-overlay-blocked column.
    if blocked(to) || !chunk_map.passable_at(to.0, to.1, to.2) {
        return false;
    }
    if blocked(from) {
        return false;
    }

    // A* over (x, y, z); g = step count, h = chebyshev(xy) to goal.
    let goal_xy = (to.0, to.1);
    let mut g: AHashMap<(i32, i32, i32), i32> = AHashMap::new();
    let mut open: BinaryHeap<Reverse<(i32, i32, (i32, i32, i32))>> = BinaryHeap::new();
    g.insert(from, 0);
    open.push(Reverse((cheby((from.0, from.1), goal_xy), 0, from)));
    let mut expansions = 0usize;

    while let Some(Reverse((_f, gc, cur))) = open.pop() {
        if (cur.0, cur.1) == goal_xy {
            return true;
        }
        if gc > *g.get(&cur).unwrap_or(&i32::MAX) {
            continue; // stale heap entry
        }
        expansions += 1;
        if expansions > opts.max_expansions {
            return false; // fail closed — never ship an unproven placement
        }
        for (dx, dy) in DIRS8 {
            let nx = cur.0 + dx;
            let ny = cur.1 + dy;
            // Probe same / up / down for the standable z of the step, exactly
            // as per-step agent movement resolves it.
            let mut stepped = None;
            for ndz in [0i32, 1, -1] {
                let nz = cur.2 + ndz;
                if nz < Z_MIN || nz > Z_MAX {
                    continue;
                }
                let nbr = (nx, ny, nz);
                if blocked(nbr) {
                    continue;
                }
                if passable_diagonal_step(chunk_map, cur, nbr) {
                    stepped = Some(nbr);
                    break;
                }
            }
            let Some(nbr) = stepped else {
                continue;
            };
            let ng = gc + 1;
            if ng < *g.get(&nbr).unwrap_or(&i32::MAX) {
                g.insert(nbr, ng);
                let h = cheby((nbr.0, nbr.1), goal_xy);
                open.push(Reverse((ng + h, ng, nbr)));
            }
        }
    }
    false
}

/// Optional O(1) runtime-only fast pre-filter over the *settled* connectivity
/// graph. `Some(false)` ⇒ provably unreachable, reject cheaply. `Some(true)` /
/// `None` ⇒ inconclusive, the caller must still confirm with [`path_exists`].
/// Returns `None` when either endpoint isn't classified yet (graph not built /
/// stale) so seed-time callers degrade to the authoritative search.
pub fn connectivity_prefilter(
    conn: &ChunkConnectivity,
    graph: &ChunkGraph,
    from: (i32, i32, i32),
    to: (i32, i32, i32),
) -> Option<bool> {
    let fz = i8::try_from(from.2.clamp(Z_MIN, Z_MAX)).ok()?;
    let tz = i8::try_from(to.2.clamp(Z_MIN, Z_MAX)).ok()?;
    // component_for_tile returns None if not classified; treat the whole query
    // as inconclusive (None) rather than "unreachable" so we never reject at
    // seed time when the graph isn't ready.
    graph.component_for_tile(from.0, from.1, fz)?;
    graph.component_for_tile(to.0, to.1, tz)?;
    Some(conn.tile_reachable(graph, (from.0, from.1, fz), (to.0, to.1, tz)))
}

/// Is the single tile reachable from the faction home on the live map?
pub fn tile_reachable_from_home(chunk_map: &ChunkMap, home: (i32, i32), tile: (i32, i32)) -> bool {
    if home == tile {
        return true;
    }
    path_exists(
        chunk_map,
        resolve3(chunk_map, home),
        resolve3(chunk_map, tile),
        ReachOpts::seed(),
    )
}

/// Is the rectangle `[min, max]` (inclusive) reachable from home? Checks a
/// representative interior cell plus the four corners — a farm plot / yard is
/// accepted only if the worker can actually walk into it.
pub fn rect_reachable_from_home(
    chunk_map: &ChunkMap,
    home: (i32, i32),
    min: (i32, i32),
    max: (i32, i32),
) -> bool {
    let cx = (min.0 + max.0) / 2;
    let cy = (min.1 + max.1) / 2;
    let probes = [
        (cx, cy),
        (min.0, min.1),
        (max.0, min.1),
        (min.0, max.1),
        (max.0, max.1),
    ];
    let home3 = resolve3(chunk_map, home);
    probes
        .iter()
        .any(|&p| path_exists(chunk_map, home3, resolve3(chunk_map, p), ReachOpts::seed()))
}

/// Validate a planned walled house *as it will exist once built*: the door's
/// doormat must connect to `home`, every interior bed must be reachable from
/// the doormat *through the door* (not over a wall), and the door↔doormat z
/// step must be a single legal move. `walls` is the perimeter tile set from
/// `walled_house_tile_plan` (the door tile is excluded by construction).
pub fn simulate_house_reachable(
    chunk_map: &ChunkMap,
    home: (i32, i32),
    doormat: (i32, i32),
    door: (i32, i32),
    walls: &AHashSet<(i32, i32)>,
    beds: &[(i32, i32)],
) -> bool {
    // Door↔doormat must be a single legal step (|Δz| ≤ 1 on resolved surface).
    let dz = chunk_map.surface_z_at(door.0, door.1);
    let mz = chunk_map.surface_z_at(doormat.0, doormat.1);
    if (dz - mz).abs() > 1 {
        return false;
    }

    let wall_overlay = |t: (i32, i32, i32)| walls.contains(&(t.0, t.1));
    let home3 = resolve3(chunk_map, home);
    let doormat3 = resolve3(chunk_map, doormat);

    // Exterior: home → doormat with the finished walls in place.
    if !path_exists(
        chunk_map,
        home3,
        doormat3,
        ReachOpts::seed().with_blocked(&wall_overlay),
    ) {
        return false;
    }
    // Interior: every bed must be reachable from the doormat — the only gap in
    // the wall ring is the door, so this proves the door connects in/out.
    beds.iter().all(|&bed| {
        path_exists(
            chunk_map,
            doormat3,
            resolve3(chunk_map, bed),
            ReachOpts::seed().with_blocked(&wall_overlay),
        )
    })
}

// ---------------------------------------------------------------------------
// Route-aware residential placement (per-tick `RoadField` + bounded off-road).
// ---------------------------------------------------------------------------

/// BFS over the road graph (carved `TileKind::Road` tiles ∪ a planned-spine
/// set) rooted at the road tile closest to `home`. Built once per faction
/// per planning tick by `organic_settlement::settlement_morphology_system`
/// and passed by reference to every residential candidate evaluation. Cost
/// scales with road tile count, **not** city radius — a 1000-tile sprawl
/// with a 300-tile road network costs the same as a 30-tile hamlet with
/// the same road density.
#[derive(Default, Debug)]
pub struct RoadField {
    /// Road tile (`x, y`) → step count along the road graph from
    /// `home_road_tile`. Missing key means the road tile is in a
    /// disconnected fragment or no road graph was built.
    pub road_steps_to_home: AHashMap<(i32, i32), u16>,
    /// The road tile the field is rooted at (closest carved/planned road to
    /// `home`). `None` when no road exists within the tiny seed search.
    pub home_road_tile: Option<(i32, i32)>,
}

/// Safety cap on `RoadField` BFS. Real road networks are 100s of tiles even
/// in mature cities; this cap is effectively infinite.
pub const MAX_ROAD_TILES: usize = 8_000;

/// Cap on `nearest_road_cost`'s bounded local A*. Anything farther from a
/// road than this (~36 m at 1.5 m/tile) is a bad residential lot.
pub const MAX_ROAD_CONNECTOR_STEPS: u16 = 24;

#[inline]
fn is_road_tile(chunk_map: &ChunkMap, planned: &AHashSet<(i32, i32)>, t: (i32, i32)) -> bool {
    if chunk_map.tile_kind_at(t.0, t.1) == Some(crate::world::tile::TileKind::Road) {
        return true;
    }
    planned.contains(&t)
}

/// Build a `RoadField` rooted at the road tile nearest to `home`. Seeds with
/// carved `Road` tiles AND the planner's `road_tiles` set (so seed-time, when
/// no road has been carved yet, the planned spine still seeds the field).
pub fn road_field_from_home(
    chunk_map: &ChunkMap,
    planned_roads: &AHashSet<(i32, i32)>,
    home: (i32, i32),
) -> RoadField {
    use std::collections::VecDeque;
    // Locate root: home itself if road, else nearest road within ring 8.
    let mut root: Option<(i32, i32)> = None;
    if is_road_tile(chunk_map, planned_roads, home) {
        root = Some(home);
    } else {
        'search: for r in 1i32..=8 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs().max(dy.abs()) != r {
                        continue;
                    }
                    let t = (home.0 + dx, home.1 + dy);
                    if is_road_tile(chunk_map, planned_roads, t) {
                        root = Some(t);
                        break 'search;
                    }
                }
            }
        }
    }
    let Some(home_road_tile) = root else {
        return RoadField::default();
    };
    // 8-connected BFS over road-only adjacency.
    let mut steps: AHashMap<(i32, i32), u16> = AHashMap::new();
    let mut q: VecDeque<(i32, i32)> = VecDeque::new();
    steps.insert(home_road_tile, 0);
    q.push_back(home_road_tile);
    while let Some(cur) = q.pop_front() {
        if steps.len() >= MAX_ROAD_TILES {
            break;
        }
        let g = *steps.get(&cur).unwrap_or(&0);
        for (dx, dy) in DIRS8 {
            let nbr = (cur.0 + dx, cur.1 + dy);
            if !is_road_tile(chunk_map, planned_roads, nbr) {
                continue;
            }
            if steps.contains_key(&nbr) {
                continue;
            }
            steps.insert(nbr, g.saturating_add(1));
            q.push_back(nbr);
        }
    }
    RoadField {
        road_steps_to_home: steps,
        home_road_tile: Some(home_road_tile),
    }
}

/// Bounded local A* from `from` (3D) to the nearest road tile (carved OR
/// planned, via the same `RoadField.road_steps_to_home` set). Returns
/// `(off_road_steps, road_tile)` or `None` when no road is within
/// `max_steps`. Cap protects worst-case ~600 expansions per candidate.
pub fn nearest_road_cost(
    chunk_map: &ChunkMap,
    field: &RoadField,
    from: (i32, i32, i32),
    max_steps: u16,
) -> Option<(u16, (i32, i32))> {
    if field.road_steps_to_home.is_empty() {
        return None;
    }
    // Fast path: caller IS a road tile already.
    if field.road_steps_to_home.contains_key(&(from.0, from.1)) {
        return Some((0, (from.0, from.1)));
    }
    let mut g: AHashMap<(i32, i32, i32), u16> = AHashMap::new();
    let mut open: BinaryHeap<Reverse<(u16, u16, (i32, i32, i32))>> = BinaryHeap::new();
    g.insert(from, 0);
    open.push(Reverse((0, 0, from)));
    let mut expansions: usize = 0;
    while let Some(Reverse((_f, gc, cur))) = open.pop() {
        if let Some(&ent) = field.road_steps_to_home.get(&(cur.0, cur.1)) {
            // Only count it as "reached" if the road tile is actually in a
            // road-connected fragment that links back to home; a saturating
            // u16::MAX entry would mean disconnected.
            let _ = ent;
            return Some((gc, (cur.0, cur.1)));
        }
        if gc > *g.get(&cur).unwrap_or(&u16::MAX) {
            continue;
        }
        if gc >= max_steps {
            continue;
        }
        expansions += 1;
        if expansions > 4_000 {
            return None;
        }
        for (dx, dy) in DIRS8 {
            let nx = cur.0 + dx;
            let ny = cur.1 + dy;
            let mut stepped = None;
            for ndz in [0i32, 1, -1] {
                let nz = cur.2 + ndz;
                if nz < Z_MIN || nz > Z_MAX {
                    continue;
                }
                let nbr = (nx, ny, nz);
                if passable_diagonal_step(chunk_map, cur, nbr) {
                    stepped = Some(nbr);
                    break;
                }
            }
            let Some(nbr) = stepped else { continue };
            let ng = gc.saturating_add(1);
            if ng < *g.get(&nbr).unwrap_or(&u16::MAX) {
                g.insert(nbr, ng);
                // Heuristic = 0 (we don't know a road's tile until found).
                open.push(Reverse((ng, ng, nbr)));
            }
        }
    }
    None
}

/// Per-candidate composite route summary used by the residential scorer.
#[derive(Clone, Copy, Debug)]
pub struct PathStats {
    pub off_road_steps: u16,
    pub on_road_steps: u16,
    pub total_steps: u16,
    /// Chebyshev distance from candidate to `home` — the "ideal" baseline
    /// against which detour is measured.
    pub direct: u16,
    pub detour: i32,
    pub detour_ratio: f32,
    /// One of the underlying searches hit its expansion cap. Treat as
    /// last-resort: a sealed pocket would have returned `None` instead.
    pub saturated: bool,
}

/// Compose `nearest_road_cost` + `RoadField.road_steps_to_home` lookup into a
/// single per-candidate score. Returns `None` when the candidate is too far
/// from any road OR the road tile reached sits in a disconnected fragment
/// (`road_steps_to_home` only stores tiles reachable from `home_road_tile`).
///
/// **Empty-roads fallback**: when `field.home_road_tile.is_none()` (very
/// first seed ticks — no road carved or planned yet), falls through to a
/// single bounded `path_exists`-style A* `home → candidate` so the planner
/// still gets a usable signal. The fallback returns `total_steps == direct`
/// when the line of sight is open.
pub fn path_stats(
    chunk_map: &ChunkMap,
    field: &RoadField,
    candidate: (i32, i32, i32),
    home: (i32, i32),
) -> Option<PathStats> {
    let direct = cheby((candidate.0, candidate.1), home).max(1) as u16;
    // Empty-roads fallback for very first seed ticks.
    if field.home_road_tile.is_none() {
        let home3 = resolve3(chunk_map, home);
        let reachable = path_exists(chunk_map, home3, candidate, ReachOpts::seed());
        if !reachable {
            return None;
        }
        // We don't know the actual step count without a second search; use
        // direct as an optimistic estimate. Detour = 0 is the right
        // semantic for "the road graph isn't built yet, score on chebyshev".
        return Some(PathStats {
            off_road_steps: 0,
            on_road_steps: direct,
            total_steps: direct,
            direct,
            detour: 0,
            detour_ratio: 1.0,
            saturated: false,
        });
    }
    let (off, road_tile) = nearest_road_cost(chunk_map, field, candidate, MAX_ROAD_CONNECTOR_STEPS)?;
    let on = *field.road_steps_to_home.get(&road_tile)?;
    let total = off.saturating_add(on);
    let detour = (total as i32) - (direct as i32);
    let detour_ratio = (total as f32) / (direct as f32);
    Some(PathStats {
        off_road_steps: off,
        on_road_steps: on,
        total_steps: total,
        direct,
        detour,
        detour_ratio,
        saturated: false,
    })
}

/// Yield up to `n` distinct tiles reachable from `home` by flooding outward
/// (so every spawned member / household tile is reachable-from-home *by
/// construction* rather than random-scatter-then-test). BFS over the canonical
/// step rules; `home` itself is included first when standable. Returns fewer
/// than `n` only when the connected open area around `home` is genuinely small.
pub fn spawn_tiles_from(chunk_map: &ChunkMap, home: (i32, i32), n: usize) -> Vec<(i32, i32)> {
    use std::collections::VecDeque;
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(n);
    if n == 0 {
        return out;
    }
    let start = resolve3(chunk_map, home);
    if !chunk_map.passable_at(start.0, start.1, start.2) {
        return out;
    }
    let mut visited: AHashSet<(i32, i32)> = AHashSet::new();
    visited.insert((start.0, start.1));
    let mut q: VecDeque<(i32, i32, i32)> = VecDeque::new();
    q.push_back(start);
    out.push((start.0, start.1));
    let cap = SEED_MAX_EXPANSIONS;
    let mut expansions = 0usize;
    while let Some(cur) = q.pop_front() {
        if out.len() >= n {
            break;
        }
        expansions += 1;
        if expansions > cap {
            break;
        }
        for (dx, dy) in DIRS8 {
            let nx = cur.0 + dx;
            let ny = cur.1 + dy;
            if visited.contains(&(nx, ny)) {
                continue;
            }
            let mut stepped = None;
            for ndz in [0i32, 1, -1] {
                let nz = cur.2 + ndz;
                if nz < Z_MIN || nz > Z_MAX {
                    continue;
                }
                let nbr = (nx, ny, nz);
                if passable_diagonal_step(chunk_map, cur, nbr) {
                    stepped = Some(nbr);
                    break;
                }
            }
            let Some(nbr) = stepped else {
                continue;
            };
            visited.insert((nx, ny));
            out.push((nx, ny));
            q.push_back(nbr);
            if out.len() >= n {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    /// Single flat grass chunk at z=0 (tiles 0..=31 in chunk (0,0)).
    fn flat_map() -> ChunkMap {
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[8u8; CHUNK_SIZE]; CHUNK_SIZE]);
        let mut map = ChunkMap::default();
        map.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    /// Make tile `(x, y)` impassable exactly the way a finished wall does:
    /// a `Wall` block in the headspace above the surface (foot stays Grass,
    /// head is no longer Air → `passable_at` is false).
    fn wall(map: &mut ChunkMap, x: i32, y: i32) {
        map.set_tile(
            x,
            y,
            1,
            TileData {
                kind: TileKind::Wall,
                ..Default::default()
            },
        );
    }

    #[test]
    fn path_exists_open_grass() {
        let map = flat_map();
        assert!(path_exists(
            &map,
            resolve3(&map, (1, 1)),
            resolve3(&map, (20, 8)),
            ReachOpts::seed()
        ));
    }

    #[test]
    fn path_exists_rejects_within_chunk_pocket() {
        // A solid wall ring around (16,16) seals it from the rest of the
        // SAME chunk — proves this is a real path search, not a coarse
        // component test.
        let mut map = flat_map();
        for d in -1..=1 {
            wall(&mut map, 16 + d, 15);
            wall(&mut map, 16 + d, 17);
            wall(&mut map, 15, 16 + d);
            wall(&mut map, 17, 16 + d);
        }
        assert!(!path_exists(
            &map,
            resolve3(&map, (1, 1)),
            resolve3(&map, (16, 16)),
            ReachOpts::seed()
        ));
        // The same two tiles ARE connected once the ring is gone.
        let open = flat_map();
        assert!(path_exists(
            &open,
            resolve3(&open, (1, 1)),
            resolve3(&open, (16, 16)),
            ReachOpts::seed()
        ));
    }

    #[test]
    fn spawn_tiles_from_yields_only_reachable() {
        // Wall off a 1-tile pocket; the pool must never include it.
        let mut map = flat_map();
        for d in -1..=1 {
            wall(&mut map, 25 + d, 24);
            wall(&mut map, 25 + d, 26);
            wall(&mut map, 24, 25 + d);
            wall(&mut map, 26, 25 + d);
        }
        let pool = spawn_tiles_from(&map, (1, 1), 40);
        assert!(pool.len() >= 40, "open area should yield the full request");
        assert!(
            !pool.contains(&(25, 25)),
            "sealed pocket tile must not be offered"
        );
        // Every yielded tile is genuinely reachable from home.
        let home = resolve3(&map, (1, 1));
        for &t in &pool {
            assert!(
                path_exists(&map, home, resolve3(&map, t), ReachOpts::seed()),
                "pool tile {t:?} not reachable from home"
            );
        }
    }

    #[test]
    fn rect_reachable_rejects_isolated_plot() {
        // Box a 4×4 plot completely in walls; unreachable from home.
        let mut map = flat_map();
        let (x0, y0, x1, y1) = (10, 10, 13, 13);
        for x in x0 - 1..=x1 + 1 {
            wall(&mut map, x, y0 - 1);
            wall(&mut map, x, y1 + 1);
        }
        for y in y0 - 1..=y1 + 1 {
            wall(&mut map, x0 - 1, y);
            wall(&mut map, x1 + 1, y);
        }
        assert!(!rect_reachable_from_home(&map, (1, 1), (x0, y0), (x1, y1)));
        // An un-walled plot of the same size is reachable.
        let open = flat_map();
        assert!(rect_reachable_from_home(&open, (1, 1), (x0, y0), (x1, y1)));
    }

    /// 5×5 walled house centred at `c`, west-edge door, one east-interior bed.
    fn house_walls(c: (i32, i32)) -> (AHashSet<(i32, i32)>, (i32, i32), (i32, i32)) {
        let mut walls = AHashSet::new();
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                if dx.abs() == 2 || dy.abs() == 2 {
                    walls.insert((c.0 + dx, c.1 + dy));
                }
            }
        }
        let door = (c.0 - 2, c.1); // west-centre ring cell → door gap
        walls.remove(&door);
        let doormat = (c.0 - 3, c.1);
        (walls, door, doormat)
    }

    #[test]
    fn simulate_house_accepts_reachable_bed() {
        let map = flat_map();
        let c = (16, 16);
        let (walls, door, doormat) = house_walls(c);
        let beds = [(c.0 + 1, c.1)]; // east interior cell
        assert!(simulate_house_reachable(
            &map,
            (1, 1),
            doormat,
            door,
            &walls,
            &beds
        ));
    }

    /// Paint a horizontal road from (x0,y) to (x1,y) inclusive at z=0.
    fn road_h(map: &mut ChunkMap, y: i32, x0: i32, x1: i32) {
        for x in x0..=x1 {
            map.set_tile(
                x,
                y,
                0,
                TileData {
                    kind: TileKind::Road,
                    ..Default::default()
                },
            );
        }
    }

    #[test]
    fn road_field_straight_line() {
        let mut map = flat_map();
        road_h(&mut map, 5, 1, 20);
        let planned = AHashSet::new();
        let field = road_field_from_home(&map, &planned, (1, 5));
        assert_eq!(field.home_road_tile, Some((1, 5)));
        for x in 1..=20 {
            let want = (x - 1) as u16;
            assert_eq!(field.road_steps_to_home.get(&(x, 5)).copied(), Some(want));
        }
        assert!(!field.road_steps_to_home.contains_key(&(5, 7)));
    }

    #[test]
    fn road_field_planned_only_no_carved() {
        let map = flat_map();
        let mut planned: AHashSet<(i32, i32)> = AHashSet::new();
        for x in 1..=10 {
            planned.insert((x, 5));
        }
        let field = road_field_from_home(&map, &planned, (1, 5));
        assert_eq!(field.home_road_tile, Some((1, 5)));
        assert!(field.road_steps_to_home.contains_key(&(10, 5)));
    }

    #[test]
    fn road_field_disconnected_fragment_absent() {
        let mut map = flat_map();
        road_h(&mut map, 5, 1, 5);
        road_h(&mut map, 5, 11, 15);
        let planned = AHashSet::new();
        let field = road_field_from_home(&map, &planned, (1, 5));
        assert!(field.road_steps_to_home.contains_key(&(5, 5)));
        assert!(!field.road_steps_to_home.contains_key(&(11, 5)));
        assert!(!field.road_steps_to_home.contains_key(&(15, 5)));
    }

    #[test]
    fn nearest_road_cost_adjacent() {
        let mut map = flat_map();
        road_h(&mut map, 5, 1, 10);
        let planned = AHashSet::new();
        let field = road_field_from_home(&map, &planned, (1, 5));
        let cand = resolve3(&map, (5, 4));
        let (off, road) = nearest_road_cost(&map, &field, cand, 24).expect("reaches road");
        assert_eq!(off, 1);
        assert_eq!(road.1, 5);
    }

    #[test]
    fn nearest_road_cost_too_far_returns_none() {
        let mut map = flat_map();
        road_h(&mut map, 5, 1, 5);
        let planned = AHashSet::new();
        let field = road_field_from_home(&map, &planned, (1, 5));
        // Candidate 25 chebyshev east of the road end; max_steps = 24 → None.
        let cand = resolve3(&map, (30, 5));
        assert!(nearest_road_cost(&map, &field, cand, 24).is_none());
    }

    #[test]
    fn path_stats_direct_lot() {
        let mut map = flat_map();
        road_h(&mut map, 5, 1, 20);
        let planned = AHashSet::new();
        let field = road_field_from_home(&map, &planned, (1, 5));
        // Lot at (10, 4): 1 off-road north of the road, ~9 along road to (1,5).
        let cand = resolve3(&map, (10, 4));
        let stats = path_stats(&map, &field, cand, (1, 5)).expect("reachable");
        assert_eq!(stats.off_road_steps, 1);
        assert!(
            (8..=10).contains(&stats.on_road_steps),
            "on_road_steps={}",
            stats.on_road_steps
        );
        assert_eq!(stats.direct, 9);
    }

    #[test]
    fn path_stats_empty_roads_fallback() {
        let map = flat_map();
        let planned = AHashSet::new();
        let field = road_field_from_home(&map, &planned, (1, 1));
        assert!(field.home_road_tile.is_none());
        let cand = resolve3(&map, (10, 10));
        let stats = path_stats(&map, &field, cand, (1, 1)).expect("open grass should reach");
        assert_eq!(stats.detour, 0);
        assert_eq!(stats.total_steps, stats.direct);
    }

    #[test]
    fn simulate_house_rejects_sealed_bed() {
        let map = flat_map();
        let c = (16, 16);
        let (mut walls, door, doormat) = house_walls(c);
        // Vertical interior divider at x = c.0 (y = c.1-1..=c.1+1) bridging
        // the top/bottom ring → the east interior is sealed from the west
        // door. Bed sits east of the divider.
        for dy in -1..=1 {
            walls.insert((c.0, c.1 + dy));
        }
        let beds = [(c.0 + 1, c.1)];
        assert!(!simulate_house_reachable(
            &map,
            (1, 1),
            doormat,
            door,
            &walls,
            &beds
        ));
        // Same house, bed on the WEST (door) side of the divider → reachable.
        let beds_ok = [(c.0 - 1, c.1)];
        assert!(simulate_house_reachable(
            &map,
            (1, 1),
            doormat,
            door,
            &walls,
            &beds_ok
        ));
    }
}
