//! Era × peak-population civic milestone table.
//!
//! Phase 5 of the Construction Overhaul replaces the `bed_count >= N`
//! proxies that previously gated Granary/Shrine/Market/Barracks/Monument
//! commissions with explicit `(Era, peak_population)` thresholds. Reading
//! peak (not current) population means a tribe that drops from 30 → 15
//! keeps its market — civic capital persists through demographic dips.
//!
//! Seeded buildings (`seed_starting_buildings_system`) bypass this gate
//! entirely; the table covers *growth* only.

use crate::simulation::technology::Era;

/// Civic-building kinds that the milestone table gates. Maps to
/// `BuildSiteKind` variants but kept as a separate enum so the table
/// doesn't pull in the construction module's full surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CivicKind {
    Granary,
    Shrine,
    Market,
    Barracks,
    Monument,
    /// Timber bridge over a river tile. Smaller scale threshold than
    /// Market — bridges are public utility, not status; settlements
    /// don't need full civic capacity to coordinate one span.
    Bridge,
}

/// Returns true iff a faction in `era` with `peak_population` may commission
/// a civic building of `kind`. Tech gates run alongside this — the table
/// answers the population question only.
pub fn civic_milestone_allows(kind: CivicKind, era: Era, peak_population: u32) -> bool {
    let (min_era, min_pop) = match kind {
        CivicKind::Granary => (Era::Neolithic, 8),
        CivicKind::Shrine => (Era::Neolithic, 20),
        CivicKind::Market => (Era::Chalcolithic, 40),
        CivicKind::Barracks => (Era::Chalcolithic, 30),
        CivicKind::Monument => (Era::BronzeAge, 80),
        CivicKind::Bridge => (Era::Chalcolithic, 20),
    };
    (era as u8) >= (min_era as u8) && peak_population >= min_pop
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paleolithic_band_blocks_every_civic() {
        for kind in [
            CivicKind::Granary,
            CivicKind::Shrine,
            CivicKind::Market,
            CivicKind::Barracks,
            CivicKind::Monument,
            CivicKind::Bridge,
        ] {
            assert!(!civic_milestone_allows(kind, Era::Paleolithic, 1000));
        }
    }

    #[test]
    fn chalcolithic_20_unlocks_bridge() {
        assert!(civic_milestone_allows(
            CivicKind::Bridge,
            Era::Chalcolithic,
            20
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Bridge,
            Era::Chalcolithic,
            19
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Bridge,
            Era::Neolithic,
            40
        ));
    }

    #[test]
    fn neolithic_8_unlocks_granary() {
        assert!(civic_milestone_allows(
            CivicKind::Granary,
            Era::Neolithic,
            8
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Granary,
            Era::Neolithic,
            7
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Shrine,
            Era::Neolithic,
            8
        ));
    }

    #[test]
    fn bronze_80_unlocks_monument() {
        assert!(civic_milestone_allows(
            CivicKind::Monument,
            Era::BronzeAge,
            80
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Monument,
            Era::Chalcolithic,
            80
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Monument,
            Era::BronzeAge,
            79
        ));
    }

    #[test]
    fn chalcolithic_40_unlocks_market() {
        assert!(civic_milestone_allows(
            CivicKind::Market,
            Era::Chalcolithic,
            40
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Market,
            Era::Neolithic,
            40
        ));
        assert!(!civic_milestone_allows(
            CivicKind::Market,
            Era::Chalcolithic,
            39
        ));
    }
}
