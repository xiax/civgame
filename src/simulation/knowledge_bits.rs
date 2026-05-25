//! Fixed-width bitset for knowledge ids.
//!
//! Replaces the legacy `u64` projections on `FactionTechs` /
//! `PersonKnowledge.{aware, learned}` so the catalog can grow past 64 entries
//! without churning every gate site. API mirrors what bit-or / shift code paths
//! used to do directly — `has(id)`, `set(id)`, `clear(id)`, `union`, `intersect`,
//! `difference`, `count`, `is_empty`, `iter`.
//!
//! Storage is `[u64; KNOWLEDGE_BITS_WORDS]`. Width is set to 128 bits, enough
//! room for the ancient-core content pass plus the foundational + belief +
//! technique entries the catalog adds in later phases. Bumping width is a
//! single-constant edit.

use super::technology::TechId;

/// Number of `u64` words in `KnowledgeBits`. 2 × 64 = 128 ids, enough for the
/// 50 current techs plus the ~70-entry ancient-core expansion.
pub const KNOWLEDGE_BITS_WORDS: usize = 2;

/// Total bit width of `KnowledgeBits`. Any `TechId` ≥ this constant panics
/// (debug) or silently rounds (release) — discovery + catalog tests assert
/// max id stays under this ceiling.
pub const KNOWLEDGE_BITS_CAPACITY: usize = KNOWLEDGE_BITS_WORDS * 64;

/// Fixed-width bitset over `KnowledgeId`. `Copy` so existing call sites that
/// passed `FactionTechs` by value (snapshot, design_techs, gossip merge) stay
/// shape-compatible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KnowledgeBits(pub [u64; KNOWLEDGE_BITS_WORDS]);

impl KnowledgeBits {
    /// Empty bitset.
    pub const EMPTY: Self = Self([0; KNOWLEDGE_BITS_WORDS]);

    /// Construct from a raw word array.
    #[inline]
    pub const fn from_words(words: [u64; KNOWLEDGE_BITS_WORDS]) -> Self {
        Self(words)
    }

    /// Construct from a `u64` (legacy single-word bag — only the low 64 ids
    /// land). Used by the compatibility wrappers that still take `u64` on the
    /// wire (reproduction inheritance, gossip snapshot).
    #[inline]
    pub const fn from_u64(lo: u64) -> Self {
        let mut words = [0u64; KNOWLEDGE_BITS_WORDS];
        words[0] = lo;
        Self(words)
    }

    /// Low 64 bits, for legacy `count_ones` UI math and serialisation. Higher
    /// ids are dropped — callers needing the full count must use `count()`.
    #[inline]
    pub const fn to_u64_lo(self) -> u64 {
        self.0[0]
    }

    #[inline]
    fn word_bit(id: TechId) -> (usize, u64) {
        let idx = id as usize;
        debug_assert!(
            idx < KNOWLEDGE_BITS_CAPACITY,
            "KnowledgeBits index {idx} exceeds capacity {KNOWLEDGE_BITS_CAPACITY}"
        );
        (idx / 64, 1u64 << (idx % 64))
    }

    /// Test whether `id` is in the set.
    #[inline]
    pub fn has(&self, id: TechId) -> bool {
        let (word, mask) = Self::word_bit(id);
        if word >= KNOWLEDGE_BITS_WORDS {
            return false;
        }
        self.0[word] & mask != 0
    }

    /// Add `id` to the set. No-op when already present.
    #[inline]
    pub fn set(&mut self, id: TechId) {
        let (word, mask) = Self::word_bit(id);
        if word >= KNOWLEDGE_BITS_WORDS {
            return;
        }
        self.0[word] |= mask;
    }

    /// Remove `id` from the set.
    #[inline]
    pub fn clear(&mut self, id: TechId) {
        let (word, mask) = Self::word_bit(id);
        if word >= KNOWLEDGE_BITS_WORDS {
            return;
        }
        self.0[word] &= !mask;
    }

    /// Population count.
    #[inline]
    pub fn count(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }

    /// True when no ids are set.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    /// Bitwise OR with another set.
    #[inline]
    pub fn union(&self, other: &Self) -> Self {
        let mut out = Self::EMPTY;
        for i in 0..KNOWLEDGE_BITS_WORDS {
            out.0[i] = self.0[i] | other.0[i];
        }
        out
    }

    /// In-place OR with another set.
    #[inline]
    pub fn union_assign(&mut self, other: &Self) {
        for i in 0..KNOWLEDGE_BITS_WORDS {
            self.0[i] |= other.0[i];
        }
    }

    /// Bitwise AND.
    #[inline]
    pub fn intersect(&self, other: &Self) -> Self {
        let mut out = Self::EMPTY;
        for i in 0..KNOWLEDGE_BITS_WORDS {
            out.0[i] = self.0[i] & other.0[i];
        }
        out
    }

    /// `self & !other` — ids in `self` not in `other`.
    #[inline]
    pub fn difference(&self, other: &Self) -> Self {
        let mut out = Self::EMPTY;
        for i in 0..KNOWLEDGE_BITS_WORDS {
            out.0[i] = self.0[i] & !other.0[i];
        }
        out
    }

    /// Iterate ids in ascending order.
    pub fn iter(&self) -> KnowledgeBitsIter {
        KnowledgeBitsIter {
            words: self.0,
            word_idx: 0,
        }
    }

    /// Build a bitset whose ids are exactly the lower `count` (one bit per id).
    /// Used by the chief-tech mask path that previously hand-rolled
    /// `(1u64 << TECH_COUNT) - 1`.
    pub fn lower_mask(count: usize) -> Self {
        let mut out = Self::EMPTY;
        let full_words = count / 64;
        for i in 0..full_words.min(KNOWLEDGE_BITS_WORDS) {
            out.0[i] = u64::MAX;
        }
        let leftover = count % 64;
        if leftover > 0 && full_words < KNOWLEDGE_BITS_WORDS {
            out.0[full_words] = (1u64 << leftover) - 1;
        }
        out
    }
}

/// Forward iterator yielding set ids in ascending order.
pub struct KnowledgeBitsIter {
    words: [u64; KNOWLEDGE_BITS_WORDS],
    word_idx: usize,
}

impl Iterator for KnowledgeBitsIter {
    type Item = TechId;

    fn next(&mut self) -> Option<Self::Item> {
        while self.word_idx < KNOWLEDGE_BITS_WORDS {
            let w = self.words[self.word_idx];
            if w == 0 {
                self.word_idx += 1;
                continue;
            }
            let bit = w.trailing_zeros();
            self.words[self.word_idx] &= !(1u64 << bit);
            return Some((self.word_idx as u32 * 64 + bit) as TechId);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_has_clear_round_trip() {
        let mut b = KnowledgeBits::default();
        assert!(!b.has(7));
        b.set(7);
        assert!(b.has(7));
        b.clear(7);
        assert!(!b.has(7));
    }

    #[test]
    fn bits_above_64_round_trip() {
        let mut b = KnowledgeBits::default();
        b.set(0);
        b.set(63);
        b.set(64);
        b.set(127);
        assert!(b.has(0));
        assert!(b.has(63));
        assert!(b.has(64));
        assert!(b.has(127));
        assert_eq!(b.count(), 4);
        let ids: Vec<_> = b.iter().collect();
        assert_eq!(ids, vec![0, 63, 64, 127]);
    }

    #[test]
    fn union_and_difference() {
        let mut a = KnowledgeBits::default();
        a.set(1);
        a.set(70);
        let mut b = KnowledgeBits::default();
        b.set(70);
        b.set(80);
        let u = a.union(&b);
        let d = a.difference(&b);
        assert_eq!(u.count(), 3);
        assert!(u.has(1) && u.has(70) && u.has(80));
        assert_eq!(d.count(), 1);
        assert!(d.has(1));
    }

    #[test]
    fn lower_mask_basic() {
        let m = KnowledgeBits::lower_mask(50);
        assert_eq!(m.count(), 50);
        assert!(m.has(49));
        assert!(!m.has(50));
        let m = KnowledgeBits::lower_mask(80);
        assert_eq!(m.count(), 80);
        assert!(m.has(64));
        assert!(m.has(79));
        assert!(!m.has(80));
        let m = KnowledgeBits::lower_mask(128);
        assert_eq!(m.count(), 128);
    }

    #[test]
    fn from_u64_compat() {
        let b = KnowledgeBits::from_u64(0b1011);
        assert!(b.has(0));
        assert!(b.has(1));
        assert!(!b.has(2));
        assert!(b.has(3));
        assert_eq!(b.to_u64_lo(), 0b1011);
    }
}
