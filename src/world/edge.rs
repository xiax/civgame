//! Edge barriers — walls/doors that sit on the boundary *between* two adjacent
//! tiles rather than occupying a whole tile. Permanent housing uses these so the
//! dwelling floor stays passable while movement + line-of-sight are blocked only
//! when crossing a wall-bearing edge.
//!
//! This module owns the pure coordinate primitive (`EdgeKey`) and the per-chunk
//! fast-read cache (`ChunkEdgeBits`) that the movement / LOS hot path consults
//! with no extra system params — mirroring how `TileKind::Wall` is a cache
//! projection of the durable `Wall` entity. The durable source of truth +
//! rich detail (material, owner faction, door entity, open state) lives in
//! `simulation::construction` (`EdgeStructureMap`); `ChunkEdgeBits` is rebuilt
//! from it on chunk load by `restamp_edge_structures_on_chunk_load`.

use super::chunk::CHUNK_SIZE;
use serde::{Deserialize, Serialize};

/// Orientation of a tile-boundary edge.
/// - `Vertical` separates two horizontally-adjacent tiles `(x,y)` | `(x+1,y)`
///   — the edge line runs vertically. It is the *East* edge of the owner `(x,y)`.
/// - `Horizontal` separates two vertically-adjacent tiles `(x,y)` | `(x,y+1)`
///   — the edge line runs horizontally. It is the *North* edge of the owner.
///
/// (`+y` is North / `+x` is East, matching `simulation::land::TileEdge`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeAxis {
    Vertical,
    Horizontal,
}

/// Canonical identity for the boundary between two adjacent tiles. `(x, y)` is
/// always the *owner* tile — the left tile for `Vertical`, the lower (south)
/// tile for `Horizontal` — so either ordering of the two neighbours maps to one
/// key. The owner tile's chunk stores this edge in its `ChunkEdgeBits` cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeKey {
    pub axis: EdgeAxis,
    pub x: i32,
    pub y: i32,
}

impl EdgeKey {
    /// Canonical key for the boundary between two tiles, or `None` when they are
    /// not orthogonally adjacent (diagonal / same / distant).
    pub fn between(a: (i32, i32), b: (i32, i32)) -> Option<EdgeKey> {
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        match (dx, dy) {
            (1, 0) | (-1, 0) => Some(EdgeKey {
                axis: EdgeAxis::Vertical,
                x: a.0.min(b.0),
                y: a.1,
            }),
            (0, 1) | (0, -1) => Some(EdgeKey {
                axis: EdgeAxis::Horizontal,
                x: a.0,
                y: a.1.min(b.1),
            }),
            _ => None,
        }
    }

    /// The two tiles this edge separates.
    pub fn tiles(self) -> ((i32, i32), (i32, i32)) {
        match self.axis {
            EdgeAxis::Vertical => ((self.x, self.y), (self.x + 1, self.y)),
            EdgeAxis::Horizontal => ((self.x, self.y), (self.x, self.y + 1)),
        }
    }

    /// The owner tile whose chunk stores this edge in its cache.
    pub fn owner_tile(self) -> (i32, i32) {
        (self.x, self.y)
    }
}

/// State of a single tile-boundary edge, as projected into the per-chunk cache.
/// Two bits wide on the wire (`to_bits`/`from_bits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeState {
    /// Nothing on the edge — passable + transparent.
    #[default]
    Open,
    /// A wall segment — blocks movement and line of sight.
    Wall,
    /// A shut door — passable (doors never block movement) but opaque.
    ClosedDoor,
    /// An open door — passable + transparent.
    OpenDoor,
}

impl EdgeState {
    pub fn to_bits(self) -> u8 {
        match self {
            EdgeState::Open => 0,
            EdgeState::Wall => 1,
            EdgeState::ClosedDoor => 2,
            EdgeState::OpenDoor => 3,
        }
    }

    pub fn from_bits(b: u8) -> EdgeState {
        match b & 0b11 {
            1 => EdgeState::Wall,
            2 => EdgeState::ClosedDoor,
            3 => EdgeState::OpenDoor,
            _ => EdgeState::Open,
        }
    }

    /// Does this edge block an agent crossing it? Only walls do — doors are
    /// always passable (matching full-tile `Door` behaviour).
    pub fn blocks_move(self) -> bool {
        matches!(self, EdgeState::Wall)
    }

    /// Is this edge opaque to line of sight (ignoring faction transparency)?
    /// Walls and shut doors block; open doors and empty edges are clear.
    pub fn blocks_los_opaque(self) -> bool {
        matches!(self, EdgeState::Wall | EdgeState::ClosedDoor)
    }

    pub fn is_wall(self) -> bool {
        matches!(self, EdgeState::Wall)
    }
}

/// Per-chunk dense edge cache. Each `(lx, ly)` cell packs the owner-tile's two
/// canonical edges into one byte: bits 0-1 = North edge, bits 2-3 = East edge.
/// ~1 KiB per chunk, allocated lazily (`Chunk::edge_bits`) only for chunks that
/// actually carry edge structures.
#[derive(Clone)]
pub struct ChunkEdgeBits {
    cells: Box<[[u8; CHUNK_SIZE]; CHUNK_SIZE]>,
}

impl Default for ChunkEdgeBits {
    fn default() -> Self {
        Self {
            cells: Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]),
        }
    }
}

impl ChunkEdgeBits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn north(&self, lx: usize, ly: usize) -> EdgeState {
        EdgeState::from_bits(self.cells[ly][lx] & 0b11)
    }

    pub fn east(&self, lx: usize, ly: usize) -> EdgeState {
        EdgeState::from_bits((self.cells[ly][lx] >> 2) & 0b11)
    }

    pub fn set_north(&mut self, lx: usize, ly: usize, s: EdgeState) {
        self.cells[ly][lx] = (self.cells[ly][lx] & !0b11) | s.to_bits();
    }

    pub fn set_east(&mut self, lx: usize, ly: usize, s: EdgeState) {
        self.cells[ly][lx] = (self.cells[ly][lx] & !0b1100) | (s.to_bits() << 2);
    }

    /// True when no edge in the chunk carries a structure (safe to drop the
    /// lazily-allocated cache).
    pub fn is_empty(&self) -> bool {
        self.cells.iter().flatten().all(|&c| c == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn between_canonicalizes_both_orderings() {
        let a = (5, 7);
        let b = (6, 7); // east neighbour
        let k1 = EdgeKey::between(a, b).unwrap();
        let k2 = EdgeKey::between(b, a).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.axis, EdgeAxis::Vertical);
        assert_eq!(k1.owner_tile(), (5, 7));

        let c = (5, 8); // north neighbour
        let kn1 = EdgeKey::between(a, c).unwrap();
        let kn2 = EdgeKey::between(c, a).unwrap();
        assert_eq!(kn1, kn2);
        assert_eq!(kn1.axis, EdgeAxis::Horizontal);
        assert_eq!(kn1.owner_tile(), (5, 7));
    }

    #[test]
    fn between_rejects_non_adjacent() {
        assert!(EdgeKey::between((0, 0), (1, 1)).is_none()); // diagonal
        assert!(EdgeKey::between((0, 0), (0, 0)).is_none()); // same
        assert!(EdgeKey::between((0, 0), (2, 0)).is_none()); // distant
    }

    #[test]
    fn tiles_round_trips_owner_and_neighbour() {
        let k = EdgeKey::between((5, 7), (6, 7)).unwrap();
        assert_eq!(k.tiles(), ((5, 7), (6, 7)));
        let k = EdgeKey::between((5, 8), (5, 7)).unwrap();
        assert_eq!(k.tiles(), ((5, 7), (5, 8)));
    }

    #[test]
    fn cache_set_get_round_trips_independently() {
        let mut bits = ChunkEdgeBits::new();
        assert!(bits.is_empty());
        bits.set_north(3, 4, EdgeState::Wall);
        bits.set_east(3, 4, EdgeState::ClosedDoor);
        assert_eq!(bits.north(3, 4), EdgeState::Wall);
        assert_eq!(bits.east(3, 4), EdgeState::ClosedDoor);
        // Other cells untouched.
        assert_eq!(bits.north(4, 4), EdgeState::Open);
        assert!(!bits.is_empty());
        // Overwrite north without disturbing east.
        bits.set_north(3, 4, EdgeState::OpenDoor);
        assert_eq!(bits.north(3, 4), EdgeState::OpenDoor);
        assert_eq!(bits.east(3, 4), EdgeState::ClosedDoor);
    }

    #[test]
    fn edge_state_semantics() {
        assert!(EdgeState::Wall.blocks_move());
        assert!(!EdgeState::ClosedDoor.blocks_move());
        assert!(!EdgeState::OpenDoor.blocks_move());
        assert!(EdgeState::Wall.blocks_los_opaque());
        assert!(EdgeState::ClosedDoor.blocks_los_opaque());
        assert!(!EdgeState::OpenDoor.blocks_los_opaque());
        assert!(!EdgeState::Open.blocks_los_opaque());
    }
}
