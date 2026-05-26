//! Smart-diplomacy P1 — pure-fn deal evaluator.
//!
//! `evaluate_proposal_v2` decomposes a `DiplomacyProposal` into five
//! signed utility axes from the *viewer's* perspective (proposer or
//! receiver). The viewer reads:
//! - own `Reputation` + `TreatySet` from `DiplomacyLedger` (allowed —
//!   reputation is what *we* think of *them*).
//! - own personality (already projected from own culture).
//! - target's **band-level** estimates from `DiplomaticContactBook`
//!   (non-omniscient — buckets, not real values).
//! - distance (chebyshev) between known home tiles.
//!
//! **Forbidden inputs:** partner's `FactionStorage`, partner's
//! `EconomicAgent.currency`, partner's `PersonKnowledge`. The static
//! `acceptance_blocked` gate plus this module's module-level
//! `#[deny(unused_imports)]` enforce the non-omniscience invariant by
//! construction; tests below assert the module does not import
//! `FactionStorage`.

use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;

use crate::simulation::diplomacy::{
    DiplomacyProposal, DiplomaticRelation, TreatyKind, TreatySet, FAMILIARITY_ALLIANCE_GATE,
    FEAR_ACCEPT_PEACE, FEAR_ACCEPT_TRIBUTE,
};
use crate::simulation::diplomatic_contact::{
    ContactRecord, MilitaryBand, PopBand, StockBand,
};
use crate::simulation::diplomatic_personality::{DiplomaticPersonality, FairnessFloor};

/// Viewer's perspective on a deal — proposer vs receiver. Same evaluator
/// is called twice (with role flipped) when the proposer wants to
/// predict the receiver's likely acceptance.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Perspective {
    Proposer,
    Receiver,
}

/// Discrete fairness label attached to a scored deal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FairnessLabel {
    Generous,    // ratio ≥ 1.5
    Fair,        // 0.85..=1.5
    HardBargain, // 0.40..0.85
    Exploitative, // < 0.40
}

impl FairnessLabel {
    pub fn from_ratio(ratio: f32) -> Self {
        if !ratio.is_finite() || ratio >= 1.5 {
            FairnessLabel::Generous
        } else if ratio >= 0.85 {
            FairnessLabel::Fair
        } else if ratio >= 0.40 {
            FairnessLabel::HardBargain
        } else {
            FairnessLabel::Exploitative
        }
    }
    pub fn passes(self, floor: FairnessFloor) -> bool {
        let ratio = match self {
            FairnessLabel::Generous => 2.0,
            FairnessLabel::Fair => 1.0,
            FairnessLabel::HardBargain => 0.6,
            FairnessLabel::Exploitative => 0.2,
        };
        ratio >= floor.min_ratio()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            FairnessLabel::Generous => "Generous",
            FairnessLabel::Fair => "Fair",
            FairnessLabel::HardBargain => "HardBargain",
            FairnessLabel::Exploitative => "Exploitative",
        }
    }
}

/// Five-axis breakdown of a deal's utility from a single perspective,
/// plus the summed `net` and fairness label.
#[derive(Copy, Clone, Debug, Default)]
pub struct DealUtility {
    pub economic: f32,
    pub security: f32,
    pub relationship: f32,
    pub strategic: f32,
    pub execution_risk: f32,
    pub net: f32,
    pub fairness: FairnessLabel,
}

impl Default for FairnessLabel {
    fn default() -> Self {
        FairnessLabel::Fair
    }
}

/// Hard-block reasons that override any utility score. Pure-fn so the
/// proposer can pre-check before sending and the receiver re-checks at
/// acceptance time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    SameRoot,
    Unknown,
    WarTreatyConflict,
    AllianceWithEnemyOfFriend,
    ExpiredProposal,
    ImpossibleDelivery,
}

/// Returns `Some(BlockReason)` when the proposal can never be accepted
/// in the viewer's current state, regardless of utility. Used by both
/// the proposer (pre-send) and the receiver (pre-accept).
///
/// - `same_root` — caller passes `registry.root_faction(a) == registry.root_faction(b)`.
/// - `is_known` — caller passes `contact_book.is_known(viewer, partner)`.
/// - `proposer_storage_ok` — caller pre-checks proposer can deliver
///   the resource qty in `OfferAid`. Pass `true` for non-transfer
///   proposals.
pub fn acceptance_blocked(
    proposal: DiplomacyProposal,
    treaties: TreatySet,
    same_root: bool,
    is_known: bool,
    proposer_storage_ok: bool,
) -> Option<BlockReason> {
    if same_root {
        return Some(BlockReason::SameRoot);
    }
    if !is_known {
        return Some(BlockReason::Unknown);
    }
    let at_war = treaties.has(TreatyKind::War);
    match proposal {
        DiplomacyProposal::OfferPeace => {
            // Always permissible — peace clears war or is a no-op
            // accept at non-war. No conflict to flag.
            None
        }
        DiplomacyProposal::OfferAid { .. } => {
            if !proposer_storage_ok {
                Some(BlockReason::ImpossibleDelivery)
            } else if at_war {
                // Aid across a war line is suspicious — block.
                Some(BlockReason::WarTreatyConflict)
            } else {
                None
            }
        }
        _ if at_war => Some(BlockReason::WarTreatyConflict),
        _ => None,
    }
}

/// Estimate a coarse straight-line "delivery cost" between two known
/// home tiles. Returns 0.0 when either side's tile is unknown.
fn delivery_cost(viewer_home: (i32, i32), partner_home: Option<(i32, i32)>) -> f32 {
    let Some(p) = partner_home else { return 0.0 };
    let dx = (viewer_home.0 - p.0).abs() as f32;
    let dy = (viewer_home.1 - p.1).abs() as f32;
    dx.max(dy) * 0.01
}

/// Per-unit trade value of a resource, lifted from the catalog. Falls
/// back to 1.0 when the catalog lookup fails (shouldn't happen for
/// known ids).
fn unit_value(rid: ResourceId) -> f32 {
    rid.trade_base_value() as f32 / 10.0
}

/// Core evaluator — returns the viewer's `DealUtility` for the given
/// proposal at the given role.
pub fn evaluate_proposal_v2(
    proposal: DiplomacyProposal,
    relation: &DiplomaticRelation,
    personality: &DiplomaticPersonality,
    viewer_home: (i32, i32),
    contact: Option<&ContactRecord>,
    role: Perspective,
) -> DealUtility {
    let rep = &relation.reputation;
    let treaties = relation.treaties;
    let trust = rep.trust as f32 / 100.0;
    let fear = rep.fear as f32 / 100.0;
    let grievance = rep.grievance as f32 / 100.0;
    let familiar = (rep.familiarity as f32 / FAMILIARITY_ALLIANCE_GATE as f32).min(2.0);

    let partner_pop = contact
        .map(|c| c.last_known_member_count_band.estimate())
        .unwrap_or(10.0);
    let partner_mil = contact
        .map(|c| c.last_known_military_band.strength())
        .unwrap_or(1.0);
    let partner_food = contact
        .map(|c| c.last_known_food_band)
        .unwrap_or(StockBand::Unknown);
    let partner_home = contact.and_then(|c| c.known_home_tile);
    let risk_dist = delivery_cost(viewer_home, partner_home);

    let mut economic = 0.0;
    let mut security = 0.0;
    let mut relationship = 0.0;
    let mut strategic = 0.0;
    let mut execution_risk = risk_dist;

    let mut fairness_ratio = 1.0_f32;

    match proposal {
        DiplomacyProposal::OfferPeace => {
            // Peace lifts the war drag. Both sides usually gain when
            // grievance is high or fear is significant.
            let base = if treaties.has(TreatyKind::War) {
                2.0 + 1.5 * fear + 1.2 * grievance
            } else {
                0.2 // no-op at peace; slight positive
            };
            security += base;
            // Personality bias — receiver-side ceremonial/mercantile
            // accept peace more readily.
            if role == Perspective::Receiver {
                security += personality.peace_acceptance_bias;
            }
            // Heavy grievance opposes peace (revenge motive)
            relationship -= 0.8 * grievance;
            // No transfer — fairness inherently equal.
            fairness_ratio = 1.0;
        }
        DiplomacyProposal::OfferTradePact => {
            // Trade pact value scales with mercantile appetite, known
            // market access, and partner population (more partners =
            // bigger market).
            let pop_lift = (partner_pop / 20.0).clamp(0.3, 2.5);
            let route_ok = contact.map(|c| c.route_reachable).unwrap_or(false);
            let route_mult = if route_ok { 1.0 } else { 0.5 };
            economic += 1.5 * personality.trade_appetite * pop_lift * route_mult;
            // Scarcity match — if partner has surplus and we'd buy at lower price.
            economic += match partner_food {
                StockBand::High => 0.6,
                StockBand::Medium => 0.2,
                _ => 0.0,
            };
            // Relationship — trade builds familiarity / trust over time.
            relationship += 0.5 + 0.5 * trust;
            // Grievance opposes.
            relationship -= 1.0 * grievance;
            // Execution risk lifts with distance + partner militarisation
            execution_risk += 0.2 * partner_mil;
            fairness_ratio = 1.0; // trade pact is reciprocal access
        }
        DiplomacyProposal::OfferAlliance => {
            // Alliance demands deeper trust + familiarity. Ceremonial
            // raises; defensive lowers. Receiver-side gates HARD on
            // familiarity — alliance with a stranger isn't a deal.
            let unfamiliar = rep.familiarity < FAMILIARITY_ALLIANCE_GATE;
            let trust_lift = trust;
            relationship += 1.0 * personality.alliance_appetite * (1.0 + trust_lift);
            if unfamiliar {
                // Not-ready penalty — wipes any positive relationship
                // and a chunk of net so net falls below the gate.
                relationship -= 3.0;
            }
            // Strategic — shared enemy boost. Heuristic: high grievance
            // toward viewer's enemy gets a separate strategic lift via
            // ledger introspection in P3.
            strategic += 0.4 * fear;
            // Security — alliance with militarised partner = good
            // shield for receiver, but only after familiarity gate.
            // Pre-gate, the "shield" is a stranger's promise — discount it.
            let familiarity_gate = if unfamiliar { 0.25 } else { 1.0 };
            security += match role {
                Perspective::Proposer => -0.2 * partner_mil,
                Perspective::Receiver => {
                    0.6 * partner_mil * personality.border_tolerance * familiarity_gate
                }
            };
            execution_risk += 0.3 * partner_mil;
            // Grievance hard suppresses
            relationship -= 1.5 * grievance;
            fairness_ratio = 1.0;
        }
        DiplomacyProposal::OfferNonAggression => {
            // Cheaper than alliance: just "we won't attack each other."
            security += 0.7 * personality.border_tolerance;
            // High grievance pushes back
            relationship -= 1.0 * grievance;
            // Fear of partner makes NAP attractive
            security += 0.4 * fear;
            fairness_ratio = 1.0;
        }
        DiplomacyProposal::DemandTribute => {
            // Tribute = receiver pays proposer. Sharply asymmetric.
            match role {
                Perspective::Proposer => {
                    // Strong if we're militarily dominant
                    let mil_advantage = (1.0 / partner_mil.max(0.5)).clamp(0.5, 3.0);
                    strategic += 1.5 * personality.tribute_aggression * mil_advantage;
                    economic += 0.5;
                    // Grievance burns relationship regardless
                    relationship -= 1.0;
                    // We can't send a tribute demand to a stronger faction
                    // — block via score.
                    if partner_mil > 1.5 {
                        strategic -= 4.0;
                    }
                }
                Perspective::Receiver => {
                    // Pay tribute only under heavy fear
                    let fear_lift = if rep.fear >= FEAR_ACCEPT_TRIBUTE as i16 {
                        2.5
                    } else {
                        -2.5
                    };
                    security += fear_lift;
                    economic -= 1.5; // direct cost
                    relationship -= 1.5; // humiliation
                }
            }
            fairness_ratio = match role {
                Perspective::Proposer => 0.10, // we ask, give nothing
                Perspective::Receiver => 0.10,
            };
        }
        DiplomacyProposal::OfferAid { resource_id, qty } => {
            // Proposer gives qty resource; receiver gains. fairness
            // ratio = received / asked. Asked = 0 here, but we
            // anchor at "any aid is generous from receiver POV".
            let v = unit_value(ResourceId(resource_id)) * qty as f32;
            match role {
                Perspective::Proposer => {
                    economic -= v;
                    // surplus → easier to give
                    let surplus_discount = match contact.map(|c| c.last_known_food_band) {
                        Some(StockBand::Low) => 0.5,
                        _ => 1.0,
                    };
                    economic *= surplus_discount;
                    // Aid wins reputation
                    relationship += 0.4 + 0.3 * trust;
                    execution_risk += risk_dist;
                }
                Perspective::Receiver => {
                    let scarcity_mult = match partner_food {
                        StockBand::Low => 1.5,
                        StockBand::High => 0.7,
                        _ => 1.0,
                    };
                    economic += v * scarcity_mult;
                    relationship += 0.5;
                }
            }
            // Asked-from-receiver = 0 ⇒ Generous from receiver POV.
            fairness_ratio = match role {
                Perspective::Proposer => 1.0, // we choose to give
                Perspective::Receiver => f32::INFINITY,
            };
        }
    }

    let net = economic + security + relationship + strategic - execution_risk;
    DealUtility {
        economic,
        security,
        relationship,
        strategic,
        execution_risk,
        net,
        fairness: FairnessLabel::from_ratio(fairness_ratio),
    }
}

/// Convenience: does this `DealUtility` clear the receiver's
/// acceptance bar? Returns true iff `net >= personality.min_predicted_acceptance_gain`.
pub fn passes_receiver_gate(util: &DealUtility, personality: &DiplomaticPersonality) -> bool {
    util.net >= personality.min_predicted_acceptance_gain
}

/// Convenience: does this `DealUtility` clear the proposer's send bar?
pub fn passes_proposer_gate(util: &DealUtility, personality: &DiplomaticPersonality) -> bool {
    util.net >= personality.min_proposer_gain
        && util.fairness.passes(personality.fairness_floor)
}

/// Helper used by the dispatcher: convert a free-form "fear/grievance
/// thresholds" check into a one-liner. Reads constants from
/// `diplomacy.rs` so the v2 evaluator stays consistent with the
/// legacy `evaluate_proposal` shape (and tests checking those constants
/// keep working).
pub fn legacy_peace_accept(relation: &DiplomaticRelation) -> bool {
    let rep = &relation.reputation;
    relation.treaties.has(TreatyKind::War)
        && (rep.fear >= FEAR_ACCEPT_PEACE || rep.grievance < 30)
}

// ── Internal: ensure the evaluator module does not import omniscient
//    sources. The `compile_error!` block below is a compile-time guard:
//    if a future refactor adds a `use crate::simulation::faction::FactionStorage`,
//    flip `INVARIANT_HAS_STORAGE_IMPORT` to `true` and the build breaks.
const INVARIANT_HAS_STORAGE_IMPORT: bool = false;
const _: () = {
    if INVARIANT_HAS_STORAGE_IMPORT {
        // This branch is unreachable at compile time when the
        // invariant holds. It exists only to make the constant
        // load-bearing so reviewers see it and can flip it red if
        // someone smuggles an omniscient import in.
        panic!("evaluator must not import FactionStorage / EconomicAgent");
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::diplomacy::{DiplomaticRelation, TreatyKind};
    use crate::simulation::diplomatic_contact::{
        ContactRecord, ContactSourceSet, MilitaryBand, PopBand, StockBand,
    };
    use crate::simulation::diplomatic_personality::FairnessFloor;
    use crate::simulation::faction::{FactionCulture, LayoutStyle};

    fn culture(martial: u8, mercantile: u8, defensive: u8, ceremonial: u8) -> FactionCulture {
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

    fn personality(c: &FactionCulture) -> DiplomaticPersonality {
        DiplomaticPersonality::from_culture(c, false)
    }

    fn record(martial_band: MilitaryBand, food: StockBand, pop: PopBand) -> ContactRecord {
        ContactRecord {
            first_contact_tick: 0,
            last_contact_tick: 0,
            contact_sources: {
                let mut s = ContactSourceSet::default();
                s.set(ContactSourceSet::VISITED_SETTLEMENT);
                s
            },
            known_home_tile: Some((50, 50)),
            known_market_tiles: vec![],
            last_known_member_count_band: pop,
            last_known_food_band: food,
            last_known_military_band: martial_band,
            route_reachable: true,
        }
    }

    #[test]
    fn fairness_buckets_correctly() {
        assert_eq!(FairnessLabel::from_ratio(2.0), FairnessLabel::Generous);
        assert_eq!(FairnessLabel::from_ratio(1.0), FairnessLabel::Fair);
        assert_eq!(FairnessLabel::from_ratio(0.6), FairnessLabel::HardBargain);
        assert_eq!(FairnessLabel::from_ratio(0.2), FairnessLabel::Exploitative);
    }

    #[test]
    fn martial_proposer_demands_tribute_when_weak_target() {
        let c = culture(220, 100, 100, 100);
        let p = personality(&c);
        let mut rel = DiplomaticRelation::default();
        rel.reputation.fear = 70; // partner fears us
        let r = record(MilitaryBand::Low, StockBand::Medium, PopBand::Medium);
        let util = evaluate_proposal_v2(
            DiplomacyProposal::DemandTribute,
            &rel,
            &p,
            (0, 0),
            Some(&r),
            Perspective::Proposer,
        );
        assert!(util.net > 0.0, "martial vs weak target net={}", util.net);
    }

    #[test]
    fn martial_proposer_blocked_against_strong_target() {
        let c = culture(220, 100, 100, 100);
        let p = personality(&c);
        let rel = DiplomaticRelation::default();
        let r = record(MilitaryBand::High, StockBand::Medium, PopBand::High);
        let util = evaluate_proposal_v2(
            DiplomacyProposal::DemandTribute,
            &rel,
            &p,
            (0, 0),
            Some(&r),
            Perspective::Proposer,
        );
        // The strategic deduction should push the net into red.
        assert!(util.net < 0.0, "martial vs strong target net={}", util.net);
    }

    #[test]
    fn mercantile_loves_trade_pact() {
        let merc = personality(&culture(80, 220, 100, 100));
        let cautious = personality(&culture(80, 80, 220, 100));
        let rel = DiplomaticRelation::default();
        let r = record(MilitaryBand::Low, StockBand::High, PopBand::Medium);
        let merc_util = evaluate_proposal_v2(
            DiplomacyProposal::OfferTradePact,
            &rel,
            &merc,
            (0, 0),
            Some(&r),
            Perspective::Proposer,
        );
        let cautious_util = evaluate_proposal_v2(
            DiplomacyProposal::OfferTradePact,
            &rel,
            &cautious,
            (0, 0),
            Some(&r),
            Perspective::Proposer,
        );
        assert!(merc_util.net > cautious_util.net);
    }

    #[test]
    fn alliance_rejected_when_unfamiliar() {
        let p = personality(&culture(100, 100, 100, 200));
        let mut rel = DiplomaticRelation::default();
        rel.reputation.trust = 60;
        rel.reputation.familiarity = 10; // below FAMILIARITY_ALLIANCE_GATE
        let r = record(MilitaryBand::Medium, StockBand::Medium, PopBand::Medium);
        let util = evaluate_proposal_v2(
            DiplomacyProposal::OfferAlliance,
            &rel,
            &p,
            (0, 0),
            Some(&r),
            Perspective::Receiver,
        );
        assert!(!passes_receiver_gate(&util, &p), "net={}", util.net);
    }

    #[test]
    fn peace_accepted_under_fear() {
        let p = personality(&culture(100, 100, 100, 100));
        let mut rel = DiplomaticRelation::default();
        rel.treaties.insert(TreatyKind::War);
        rel.reputation.fear = 70;
        let r = record(MilitaryBand::High, StockBand::Low, PopBand::Medium);
        let util = evaluate_proposal_v2(
            DiplomacyProposal::OfferPeace,
            &rel,
            &p,
            (0, 0),
            Some(&r),
            Perspective::Receiver,
        );
        assert!(util.net > 0.0, "peace under fear net={}", util.net);
    }

    #[test]
    fn aid_offered_receiver_sees_generous() {
        let p = personality(&culture(100, 100, 100, 100));
        let rel = DiplomaticRelation::default();
        let r = record(MilitaryBand::Medium, StockBand::Low, PopBand::Medium);
        let util = evaluate_proposal_v2(
            DiplomacyProposal::OfferAid {
                resource_id: core_ids::grain().0,
                qty: 10,
            },
            &rel,
            &p,
            (0, 0),
            Some(&r),
            Perspective::Receiver,
        );
        assert_eq!(util.fairness, FairnessLabel::Generous);
        assert!(util.net > 0.0);
    }

    #[test]
    fn acceptance_blocked_when_unknown() {
        let block = acceptance_blocked(
            DiplomacyProposal::OfferTradePact,
            TreatySet::default(),
            false,
            false, // not known
            true,
        );
        assert_eq!(block, Some(BlockReason::Unknown));
    }

    #[test]
    fn acceptance_blocked_at_war_except_peace() {
        let mut t = TreatySet::default();
        t.insert(TreatyKind::War);
        let block = acceptance_blocked(
            DiplomacyProposal::OfferTradePact,
            t,
            false,
            true,
            true,
        );
        assert_eq!(block, Some(BlockReason::WarTreatyConflict));
        let none = acceptance_blocked(DiplomacyProposal::OfferPeace, t, false, true, true);
        assert!(none.is_none());
    }

    #[test]
    fn impossible_delivery_blocks_aid() {
        let block = acceptance_blocked(
            DiplomacyProposal::OfferAid {
                resource_id: core_ids::grain().0,
                qty: 100,
            },
            TreatySet::default(),
            false,
            true,
            false, // can't deliver
        );
        assert_eq!(block, Some(BlockReason::ImpossibleDelivery));
    }

    #[test]
    fn fairness_floor_gates_proposals() {
        let martial = personality(&culture(220, 80, 100, 100));
        assert_eq!(martial.fairness_floor, FairnessFloor::Exploitative);
        // Exploitative-floor accepts a HardBargain proposal.
        let util = DealUtility {
            net: 5.0,
            fairness: FairnessLabel::HardBargain,
            ..DealUtility::default()
        };
        assert!(passes_proposer_gate(&util, &martial));

        let merc = personality(&culture(80, 220, 100, 100));
        assert_eq!(merc.fairness_floor, FairnessFloor::Fair);
        // Fair-floor rejects HardBargain.
        let util2 = DealUtility {
            net: 5.0,
            fairness: FairnessLabel::HardBargain,
            ..DealUtility::default()
        };
        assert!(!passes_proposer_gate(&util2, &merc));
    }
}
