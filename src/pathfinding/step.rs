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
