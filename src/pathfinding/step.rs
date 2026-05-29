use crate::pathfinding::tile_cost::TraversalProfile;
use crate::world::chunk::ChunkMap;

/// True iff `from → to` is a single legal 3D step AND, for diagonals,
/// BOTH cardinal corners are routable at some Z within ±1 of `from.z`.
/// Each corner is tested independently: there must exist `cz` such that
/// `passable_step_3d(from, corner@cz)` AND `passable_step_3d(corner@cz, to)`.
///
/// Per-frame movement rounds the agent's continuous position through one
/// of the two cardinal corners depending on sub-pixel timing — if either
/// corner is unreachable from the current Z, or unreachable from the
/// chosen target Z, the runtime boundary check rejects the cross and the
/// agent snap-backs.
pub fn passable_diagonal_step(
    chunk_map: &ChunkMap,
    from: (i32, i32, i32),
    to: (i32, i32, i32),
) -> bool {
    if !chunk_map.passable_step_3d(from, to) {
        return false;
    }
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    if dx == 0 || dy == 0 {
        return true;
    }
    let corner_ok = |cx: i32, cy: i32| {
        [0i32, 1, -1].iter().any(|&cdz| {
            let cz = from.2 + cdz;
            chunk_map.passable_step_3d(from, (cx, cy, cz))
                && chunk_map.passable_step_3d((cx, cy, cz), to)
        })
    };
    corner_ok(to.0, from.1) && corner_ok(from.0, to.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, CHUNK_SIZE};
    use crate::world::edge::{EdgeKey, EdgeState};
    use crate::world::tile::TileKind;

    fn flat_map() -> ChunkMap {
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        m
    }

    #[test]
    fn diagonal_blocked_when_a_corner_edge_is_walled() {
        let mut m = flat_map();
        // Open ground: NE diagonal from (5,5) to (6,6) is allowed.
        assert!(passable_diagonal_step(&m, (5, 5, 0), (6, 6, 0)));
        // Wall the east edge of (5,5) (between (5,5) and (6,5)). The rounding
        // route through corner (6,5) is now sealed → diagonal rejected (the
        // conservative both-routes-clear rule).
        let key = EdgeKey::between((5, 5), (6, 5)).unwrap();
        m.set_edge_state(key, EdgeState::Wall);
        assert!(!passable_diagonal_step(&m, (5, 5, 0), (6, 6, 0)));
    }

    #[test]
    fn cardinal_step_blocked_by_edge_wall() {
        let mut m = flat_map();
        let key = EdgeKey::between((5, 5), (6, 5)).unwrap();
        m.set_edge_state(key, EdgeState::Wall);
        assert!(!passable_diagonal_step(&m, (5, 5, 0), (6, 5, 0)));
        // A door on the same edge does not block the cardinal step.
        m.set_edge_state(key, EdgeState::ClosedDoor);
        assert!(passable_diagonal_step(&m, (5, 5, 0), (6, 5, 0)));
    }
}

/// Profile-aware `passable_diagonal_step`. `Land` behaves identically to
/// the historical function; `Amphibious` validates every leg via
/// `passable_step_for`, so a swimmer may corner through water cells.
pub fn passable_diagonal_step_for(
    chunk_map: &ChunkMap,
    from: (i32, i32, i32),
    to: (i32, i32, i32),
    profile: TraversalProfile,
) -> bool {
    if !chunk_map.passable_step_for(from, to, profile) {
        return false;
    }
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    if dx == 0 || dy == 0 {
        return true;
    }
    let corner_ok = |cx: i32, cy: i32| {
        [0i32, 1, -1].iter().any(|&cdz| {
            let cz = from.2 + cdz;
            chunk_map.passable_step_for(from, (cx, cy, cz), profile)
                && chunk_map.passable_step_for((cx, cy, cz), to, profile)
        })
    };
    corner_ok(to.0, from.1) && corner_ok(from.0, to.1)
}
