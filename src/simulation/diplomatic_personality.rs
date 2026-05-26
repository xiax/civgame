//! Smart-diplomacy P1 — pure projection of `FactionCulture` into a bag of
//! diplomatic decision weights. Re-derived every cycle (no stored state)
//! so culture drift over generations propagates without bookkeeping.
//!
//! Inputs are the four `FactionCulture` axes (`martial / mercantile /
//! defensive / ceremonial`, each 0..=255) plus the home lifestyle bit
//! (mobile vs settled, lifted off `FactionData.caps.home`). Outputs are
//! `f32` weights consumed by `diplomatic_evaluator::evaluate_proposal_v2`
//! and `diplomacy::ai_diplomacy_proposal_system`.

use crate::simulation::faction::FactionCulture;

/// Bag of utility weights derived from a faction's culture + caps.
///
/// All weights are in human-readable ranges (multipliers near 1.0,
/// thresholds in the same units as `DealUtility.net`) so call sites can
/// read the values out as-is without re-normalisation.
#[derive(Copy, Clone, Debug)]
pub struct DiplomaticPersonality {
    // ── Motive biases ──────────────────────────────────────────────
    /// Multiplier on `economic_value` when scoring a Trade Pact for
    /// this faction. Mercantile cultures crank this up.
    pub trade_appetite: f32,
    /// Multiplier on `relationship_value` for Alliance offers.
    /// Ceremonial high, defensive low.
    pub alliance_appetite: f32,
    /// Multiplier on `security_value` for tribute *demands* (proposer
    /// side). Martial high.
    pub tribute_aggression: f32,

    // ── Risk thresholds ────────────────────────────────────────────
    /// Minimum `DealUtility.net` for the proposer side to consider
    /// posting a proposal. Martial factions tolerate riskier offers
    /// (lower threshold); cautious factions raise this.
    pub min_proposer_gain: f32,
    /// Minimum predicted `DealUtility.net` for the receiver side that
    /// the AI demands before sending the proposal. Mirrors the
    /// receiver's actual acceptance gate.
    pub min_predicted_acceptance_gain: f32,
    /// Fairness floor (P3 multi-term sends drop below this). Mercantile
    /// raises; martial lowers (HardBargain ok).
    pub fairness_floor: FairnessFloor,

    // ── Trespass / border policy ───────────────────────────────────
    /// Extra incidents granted before escalation kicks in. Defensive /
    /// martial = 0 (warn immediately on first repeat); mercantile = +1
    /// (extra grace for traders).
    pub trespass_warn_grace: i8,
    /// Multiplier on `security_value` for NAP / Alliance — defensive
    /// cultures value safe borders more. Read by evaluator.
    pub border_tolerance: f32,

    // ── Acceptance bias on receive side ────────────────────────────
    /// Added to the receiver's net gain when evaluating an incoming
    /// `OfferPeace` — ceremonial / mercantile lean toward peace
    /// (positive bias = easier to accept), martial lean away.
    pub peace_acceptance_bias: f32,
}

/// Coarse fairness-floor enum. Mapped to a numeric ratio at the
/// evaluator boundary so the personality type can stay enum-driven
/// (cheap derive + debug).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FairnessFloor {
    Exploitative,
    HardBargain,
    Fair,
}

impl FairnessFloor {
    /// Lower bound on `DealUtility.fairness_ratio` an offer must clear.
    /// `0.0` = anything goes (Exploitative ok).
    pub fn min_ratio(self) -> f32 {
        match self {
            FairnessFloor::Exploitative => 0.0,
            FairnessFloor::HardBargain => 0.40,
            FairnessFloor::Fair => 0.85,
        }
    }
}

impl DiplomaticPersonality {
    /// Pure projection from `FactionCulture` + lifestyle. No state, no
    /// side effects. `home_is_mobile` should be lifted from
    /// `FactionData.caps.home.is_mobile()` at the call site.
    pub fn from_culture(culture: &FactionCulture, home_is_mobile: bool) -> Self {
        let martial = culture.martial as f32 / 255.0;
        let mercantile = culture.mercantile as f32 / 255.0;
        let defensive = culture.defensive as f32 / 255.0;
        let ceremonial = culture.ceremonial as f32 / 255.0;

        // Trade — anchored at 1.0, lifted up to +0.8 by mercantile,
        // dampened up to -0.3 by martial.
        let trade_appetite = (1.0 + 0.8 * mercantile - 0.3 * martial).max(0.1);

        // Alliance — ceremonial main driver, defensive damps it
        // (defensive prefers NAP over entangling alliances).
        let alliance_appetite = (1.0 + 0.7 * ceremonial - 0.4 * defensive).max(0.1);

        // Tribute demands — martial only.
        let tribute_aggression = (0.4 + 1.2 * martial).max(0.1);

        // Proposer-gain threshold:
        //   martial → ~0.5 (loose, posts riskier offers)
        //   default → 1.5
        //   defensive → ~2.2 (only sends sure-thing proposals)
        let min_proposer_gain = 1.5 - 1.0 * martial + 0.7 * defensive;

        // Receiver-side prediction threshold — receivers are picky,
        // so we predict 1.0+ even at baseline.
        let min_predicted_acceptance_gain = 1.0;

        // Fairness floor — discrete enum, picked by dominant axis.
        let fairness_floor = if martial > 0.6 && martial > mercantile {
            FairnessFloor::Exploitative
        } else if mercantile > 0.55 {
            FairnessFloor::Fair
        } else {
            FairnessFloor::HardBargain
        };

        // Trespass grace: defensive/martial = 0, neutral = 0, mercantile = +1
        let trespass_warn_grace = if mercantile > 0.55 { 1 } else { 0 };

        // Border tolerance — defensive raises; nomadic lifestyle lowers
        // (nomads have softer borders by definition).
        let mut border_tolerance = 1.0 + 0.8 * defensive;
        if home_is_mobile {
            border_tolerance *= 0.6;
        }
        // High-density (compact-style) cultures defend tighter — raise
        // when density is high.
        border_tolerance *= 1.0 + 0.3 * (culture.density as f32 / 255.0);

        // Peace acceptance — ceremonial pacifist + mercantile both lean toward peace
        let peace_acceptance_bias = 0.4 * ceremonial + 0.3 * mercantile - 0.5 * martial;

        DiplomaticPersonality {
            trade_appetite,
            alliance_appetite,
            tribute_aggression,
            min_proposer_gain,
            min_predicted_acceptance_gain,
            fairness_floor,
            trespass_warn_grace,
            border_tolerance,
            peace_acceptance_bias,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::faction::{FactionCulture, LayoutStyle};

    fn mock_culture(martial: u8, mercantile: u8, defensive: u8, ceremonial: u8) -> FactionCulture {
        FactionCulture {
            style: LayoutStyle::Radial,
            density: 128,
            defensive,
            ceremonial,
            mercantile,
            martial,
            seed: 0,
        }
    }

    #[test]
    fn martial_lowers_proposer_threshold() {
        let martial = DiplomaticPersonality::from_culture(&mock_culture(220, 100, 100, 100), false);
        let cautious = DiplomaticPersonality::from_culture(&mock_culture(50, 100, 200, 100), false);
        assert!(martial.min_proposer_gain < cautious.min_proposer_gain);
    }

    #[test]
    fn mercantile_raises_trade_appetite() {
        let merc = DiplomaticPersonality::from_culture(&mock_culture(80, 220, 100, 100), false);
        let martial = DiplomaticPersonality::from_culture(&mock_culture(220, 80, 100, 100), false);
        assert!(merc.trade_appetite > martial.trade_appetite);
    }

    #[test]
    fn ceremonial_raises_alliance_appetite() {
        let cer = DiplomaticPersonality::from_culture(&mock_culture(80, 100, 80, 220), false);
        let def = DiplomaticPersonality::from_culture(&mock_culture(80, 100, 220, 80), false);
        assert!(cer.alliance_appetite > def.alliance_appetite);
    }

    #[test]
    fn mercantile_floor_is_fair() {
        let p = DiplomaticPersonality::from_culture(&mock_culture(50, 220, 100, 100), false);
        assert_eq!(p.fairness_floor, FairnessFloor::Fair);
    }

    #[test]
    fn martial_floor_is_exploitative() {
        let p = DiplomaticPersonality::from_culture(&mock_culture(220, 80, 100, 100), false);
        assert_eq!(p.fairness_floor, FairnessFloor::Exploitative);
    }

    #[test]
    fn nomadic_lowers_border_tolerance() {
        let c = mock_culture(100, 100, 200, 100);
        let settled = DiplomaticPersonality::from_culture(&c, false);
        let nomadic = DiplomaticPersonality::from_culture(&c, true);
        assert!(nomadic.border_tolerance < settled.border_tolerance);
    }

    #[test]
    fn martial_lowers_peace_bias() {
        let martial = DiplomaticPersonality::from_culture(&mock_culture(220, 100, 100, 100), false);
        let pacifist = DiplomaticPersonality::from_culture(&mock_culture(50, 100, 100, 220), false);
        assert!(pacifist.peace_acceptance_bias > martial.peace_acceptance_bias);
    }
}
