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

use crate::game_state::StartSettlementMaturity;
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
    /// Stone-and-timber dam impounding a watercourse. Bronze-Age hydraulic
    /// engineering — heavier coordination than a bridge (it reshapes the
    /// watershed) but pure utility, so below the Monument status threshold.
    Dam,
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
        CivicKind::Dam => (Era::BronzeAge, 30),
    };
    (era as u8) >= (min_era as u8) && peak_population >= min_pop
}

/// Seed-time wrapper around `civic_milestone_allows` that folds in the
/// player-chosen `StartSettlementMaturity`. Used by `generate_candidates`
/// when running under `seed_techs.is_some()` to decide whether a civic
/// kind is allowed despite under-threshold pop.
///
/// - `Founder` re-imposes the runtime gates (no civic-seeding bypass).
/// - `Established` keeps the legacy "society in progress" behaviour:
///   any era-appropriate civic seeds regardless of pop.
/// - `Developed` matches `Established`, plus an explicit override that
///   force-enables Market/Barracks/Monument for Chalcolithic+ starts
///   even when peak_pop falls below the Bronze milestones.
pub fn should_seed_civic(
    kind: CivicKind,
    era: Era,
    peak_population: u32,
    maturity: StartSettlementMaturity,
    seed_mode: bool,
) -> bool {
    if !seed_mode {
        return civic_milestone_allows(kind, era, peak_population);
    }
    match maturity {
        StartSettlementMaturity::Founder => civic_milestone_allows(kind, era, peak_population),
        StartSettlementMaturity::Established => true,
        StartSettlementMaturity::Developed => {
            if (era as u8) >= (Era::Chalcolithic as u8)
                && matches!(
                    kind,
                    CivicKind::Market | CivicKind::Barracks | CivicKind::Monument
                )
            {
                return true;
            }
            true
        }
    }
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
            CivicKind::Dam,
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
    fn bronze_30_unlocks_dam_chalcolithic_does_not() {
        assert!(civic_milestone_allows(CivicKind::Dam, Era::BronzeAge, 30));
        assert!(!civic_milestone_allows(CivicKind::Dam, Era::BronzeAge, 29));
        // A bridge-capable Chalcolithic town still can't dam (tech + civic
        // both gate later than Bridge).
        assert!(!civic_milestone_allows(
            CivicKind::Dam,
            Era::Chalcolithic,
            200
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

    #[test]
    fn founder_maturity_reimposes_runtime_gates() {
        // Neolithic-20 Founder start skips Market/Barracks/Monument
        // exactly as runtime would.
        for kind in [CivicKind::Market, CivicKind::Barracks, CivicKind::Monument] {
            assert!(!should_seed_civic(
                kind,
                Era::Neolithic,
                20,
                StartSettlementMaturity::Founder,
                true,
            ));
        }
        // But Granary (Neo, 8) still seeds for a 20-pop Neolithic Founder.
        assert!(should_seed_civic(
            CivicKind::Granary,
            Era::Neolithic,
            20,
            StartSettlementMaturity::Founder,
            true,
        ));
    }

    #[test]
    fn established_matches_legacy_seed_bypass() {
        // Established seeds Market regardless of pop / era milestone.
        assert!(should_seed_civic(
            CivicKind::Market,
            Era::Neolithic,
            20,
            StartSettlementMaturity::Established,
            true,
        ));
    }

    #[test]
    fn developed_forces_civics_chalcolithic_plus() {
        // Chalco-20 Developed start emits Market+Barracks regardless of
        // under-threshold pop.
        for kind in [CivicKind::Market, CivicKind::Barracks, CivicKind::Monument] {
            assert!(should_seed_civic(
                kind,
                Era::Chalcolithic,
                20,
                StartSettlementMaturity::Developed,
                true,
            ));
        }
    }

    #[test]
    fn non_seed_mode_falls_through_to_runtime_gate() {
        assert!(!should_seed_civic(
            CivicKind::Market,
            Era::Chalcolithic,
            39,
            StartSettlementMaturity::Established,
            false,
        ));
        assert!(should_seed_civic(
            CivicKind::Market,
            Era::Chalcolithic,
            40,
            StartSettlementMaturity::Established,
            false,
        ));
    }
}
