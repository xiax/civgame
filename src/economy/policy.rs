//! Per-resource economic policy flags. Pluralist Economy R4.
//!
//! Each `FactionData` carries an `economic_policy` map that lists, per
//! `ResourceId`, a set of composable flags describing how that
//! resource is governed inside the faction:
//!
//! - `chief_allocates_labor`: chief posts jobs (today's default).
//! - `private_actors_allowed`: households / individuals can produce
//!   and sell privately, post P2P contracts.
//! - `state_sells_at_market`: a state-owned producer (e.g. nationalised
//!   smithy) sells output at the regional market alongside private
//!   actors.
//! - `prices_fixed_by_state`: command-economy mode — the chief or
//!   bureaucrat sets a fixed price; market `update_prices` honors the
//!   override.
//! - `fixed_price`: when `prices_fixed_by_state == true`, the price the
//!   state mandates.
//!
//! Communism = all entries with `chief_allocates_labor=true,
//! private_actors_allowed=false`. Capitalism = the opposite. Wartime =
//! flip Weapons to `chief_allocates_labor=true,
//! prices_fixed_by_state=true` while leaving everything else free.
//! Mixed economies are arbitrary combinations.
//!
//! `Default::default()` returns the **all-communist** policy so any
//! resource not explicitly listed in a faction's `economic_policy` map
//! falls through to today's behaviour. This keeps existing 287 tests
//! green: a faction with an empty `economic_policy` map is
//! observationally identical to a pre-R4 faction.

use crate::economy::resource_catalog::ResourceId;

/// Per-resource control policy. See module docs for flag semantics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceControlPolicy {
    pub chief_allocates_labor: bool,
    pub private_actors_allowed: bool,
    pub state_sells_at_market: bool,
    pub prices_fixed_by_state: bool,
    pub fixed_price: Option<f32>,
}

impl Default for ResourceControlPolicy {
    /// All-communist default: chief allocates, no private actors, no
    /// state market participation, no price fixing. Matches today's
    /// observable behaviour for every existing faction.
    fn default() -> Self {
        Self {
            chief_allocates_labor: true,
            private_actors_allowed: false,
            state_sells_at_market: false,
            prices_fixed_by_state: false,
            fixed_price: None,
        }
    }
}

impl ResourceControlPolicy {
    /// Capitalist preset: no chief allocation, private actors allowed,
    /// state stays out of the market. Used as the household default
    /// (R3) and as the test-fixture preset for trader / contract / P2P
    /// scenarios.
    pub fn capitalist() -> Self {
        Self {
            chief_allocates_labor: false,
            private_actors_allowed: true,
            state_sells_at_market: false,
            prices_fixed_by_state: false,
            fixed_price: None,
        }
    }

    /// Mixed preset: chief still allocates labour, but private actors may
    /// also produce/sell. Used by the `EconomyPreset::Mixed` game-start
    /// option for non-staple resources.
    pub fn mixed() -> Self {
        Self {
            chief_allocates_labor: true,
            private_actors_allowed: true,
            state_sells_at_market: false,
            prices_fixed_by_state: false,
            fixed_price: None,
        }
    }

    /// Check whether this policy satisfies a specific required flag.
    pub fn satisfies(&self, flag: RequiredFlag) -> bool {
        match flag {
            RequiredFlag::ChiefAllocatesLabor => self.chief_allocates_labor,
            RequiredFlag::PrivateActorsAllowed => self.private_actors_allowed,
            RequiredFlag::StateSellsAtMarket => self.state_sells_at_market,
            RequiredFlag::PricesFixedByState => self.prices_fixed_by_state,
        }
    }
}

/// Flag a method declares as a precondition. The dispatcher resolves
/// the agent's faction, looks up `economic_policy.policy_for(rid)`,
/// and rejects the method if `policy.satisfies(flag) == false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequiredFlag {
    ChiefAllocatesLabor,
    PrivateActorsAllowed,
    StateSellsAtMarket,
    PricesFixedByState,
}

/// One entry in a method's policy gate: "to fire, the agent's
/// faction must have `flag` set on its policy for `resource`".
pub type PolicyGateEntry = (ResourceId, RequiredFlag);

/// Apply a `GameStartOptions::EconomyPreset` to a faction's
/// `economic_policy` map. Called once per faction by `spawn_population`.
///
/// - `Subsistence`: leave the map empty — every resource falls through to
///   the all-communist default (today's pre-pluralist behaviour).
/// - `Mixed`: insert `mixed()` (chief + private both allowed) for every
///   non-staple resource. Wood, Stone, and edibles stay communal.
/// - `Market`: insert `capitalist()` for every catalog resource.
pub fn apply_preset(
    map: &mut ahash::AHashMap<ResourceId, ResourceControlPolicy>,
    preset: crate::game_state::EconomyPreset,
    catalog: &crate::economy::resource_catalog::ResourceCatalog,
) {
    use crate::game_state::EconomyPreset;
    match preset {
        EconomyPreset::Subsistence => {}
        EconomyPreset::Mixed => {
            let wood = crate::economy::core_ids::wood();
            let stone = crate::economy::core_ids::stone();
            for (id, _def) in catalog.iter() {
                if id == wood || id == stone || id.is_edible() {
                    continue;
                }
                map.insert(id, ResourceControlPolicy::mixed());
            }
        }
        EconomyPreset::Market => {
            for (id, _def) in catalog.iter() {
                map.insert(id, ResourceControlPolicy::capitalist());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_communist() {
        let p = ResourceControlPolicy::default();
        assert!(p.chief_allocates_labor);
        assert!(!p.private_actors_allowed);
        assert!(!p.state_sells_at_market);
        assert!(!p.prices_fixed_by_state);
        assert_eq!(p.fixed_price, None);
    }

    #[test]
    fn capitalist_is_inverse_of_default_on_labor_axis() {
        let p = ResourceControlPolicy::capitalist();
        assert!(!p.chief_allocates_labor);
        assert!(p.private_actors_allowed);
    }

    #[test]
    fn satisfies_returns_correct_flag() {
        let mut p = ResourceControlPolicy::default();
        assert!(p.satisfies(RequiredFlag::ChiefAllocatesLabor));
        assert!(!p.satisfies(RequiredFlag::PrivateActorsAllowed));
        p.private_actors_allowed = true;
        assert!(p.satisfies(RequiredFlag::PrivateActorsAllowed));
    }
}
