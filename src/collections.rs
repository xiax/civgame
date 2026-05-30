//! Deterministic hash collections.
//!
//! `ahash`'s default `RandomState` (used by `ahash::AHashMap::default()` /
//! `::new()`) seeds from a per-process random source **plus** a per-instance
//! construction counter, so iteration order — and any simulation decision that
//! walks a map and picks "first/nearest" — depends on how many maps were built
//! before it in the process and on the process's random base. That made the
//! heavy behavioural tests flake (a different subset each run, in parallel
//! *and* serial), and means the game was not reproducible from its world seed.
//!
//! These aliases back `AHashMap` / `AHashSet` with [`FixedState`] — the same
//! fast `ahash` algorithm, but keyed from a **fixed** seed quad with no random
//! base and no per-instance counter. Iteration order becomes a pure function of
//! the contents, identical every run and independent of construction order. The
//! whole simulation is now deterministic from its seed (a prerequisite for
//! replays / lockstep multiplayer), and the parallel-test flakiness is removed
//! at its source.
//!
//! Use these in place of `ahash::AHashMap` / `ahash::AHashSet` everywhere in
//! the crate. `ahash::RandomState::with_seeds(..)` / `ahash::AHasher` stay the
//! deterministic primitives for the few hand-seeded sites (`net`, `region`).

use core::hash::BuildHasher;

/// Fixed seed quad for the project-wide deterministic hasher. Arbitrary but
/// constant — the only requirement is that it never changes and never derives
/// from process-random state. Distinct from `net`'s client-id seed so the two
/// determinism domains can't be confused.
const FIXED_SEEDS: (u64, u64, u64, u64) = (
    0x243F_6A88_85A3_08D3,
    0x1319_8A2E_0370_7344,
    0xA409_3822_299F_31D0,
    0x082E_FA98_EC4E_6C89,
);

/// Zero-sized, `Default`/`Copy` [`BuildHasher`] that always produces an
/// `ahash` hasher keyed from [`FIXED_SEEDS`]. Because it carries no per-instance
/// state, every map of the same contents hashes and iterates identically across
/// runs and regardless of when it was constructed.
#[derive(Default, Clone, Copy, Debug)]
pub struct FixedState;

impl BuildHasher for FixedState {
    type Hasher = ahash::AHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        let (a, b, c, d) = FIXED_SEEDS;
        // `with_seeds` uses the supplied seeds verbatim — no process randomness,
        // no global counter — so this is fully deterministic.
        ahash::RandomState::with_seeds(a, b, c, d).build_hasher()
    }
}

/// Deterministic drop-in for `ahash::AHashMap`.
pub type AHashMap<K, V> = std::collections::HashMap<K, V, FixedState>;

/// Deterministic drop-in for `ahash::AHashSet`.
pub type AHashSet<T> = std::collections::HashSet<T, FixedState>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iteration_order_is_stable_across_construction_order() {
        // Build the "same" map twice, with unrelated maps constructed in
        // between, and confirm the iteration order is identical. With ahash's
        // default RandomState the interleaved construction would shift the
        // per-instance seed and could reorder iteration; FixedState cannot.
        let build = || {
            let mut m: AHashMap<u32, u32> = AHashMap::default();
            for i in 0..64u32 {
                m.insert(i.wrapping_mul(2_654_435_761), i);
            }
            m.into_iter().collect::<Vec<_>>()
        };
        let first = build();
        // Construct noise maps to advance any hypothetical global counter.
        for _ in 0..100 {
            let mut noise: AHashMap<u64, u64> = AHashMap::default();
            noise.insert(1, 1);
            std::hint::black_box(&noise);
        }
        let second = build();
        assert_eq!(first, second, "iteration order must be construction-order-independent");
    }

    #[test]
    fn set_alias_constructs_and_inserts() {
        let mut s: AHashSet<i32> = AHashSet::default();
        s.insert(7);
        assert!(s.contains(&7));
    }
}
