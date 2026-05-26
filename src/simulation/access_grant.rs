//! Smart-diplomacy P2 — directional access grants.
//!
//! P1 trespass treated `TradePact` / `Alliance` / `NonAggression` as
//! "full territory permission" — every actor of any role was waved
//! through. That's wrong: a trade pact gives merchants market access,
//! not war bands free transit.
//!
//! `AccessGrantTable` replaces blanket treaty permission with
//! per-`(grantor, grantee)` typed grants. Treaty acceptances spawn the
//! corresponding grants automatically:
//! - `TradePact` → `MarketCorridor { settlement_id, radius }` for each of
//!   grantor's settlements.
//! - `Alliance` → `FullTerritory` (same as P1 alliance semantics).
//! - `NonAggression` → `SafePassage { until_tick: None }` — civilian
//!   transit but no harvest/build.
//!
//! Permission is intent-aware: a `Hostile` actor (drafted / raid party)
//! always trespasses regardless of grants. `permits(grantor, grantee,
//! intent, tile, settlements, calendar)` is pure-fn and consulted by
//! `trespass_detection_system`.

use ahash::AHashMap;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::simulation::faction::SOLO;
use crate::simulation::settlement::{Settlement, SettlementId};
use crate::world::seasons::Season;

/// Cardinal-bit mask of seasons during which a `SeasonalCamp` grant is
/// active. Defaults to "all seasons" so v1 grants never expire on the
/// calendar wheel unless the granter explicitly tightens.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SeasonSet(pub u8);

impl SeasonSet {
    pub const SPRING: u8 = 1 << 0;
    pub const SUMMER: u8 = 1 << 1;
    pub const AUTUMN: u8 = 1 << 2;
    pub const WINTER: u8 = 1 << 3;
    pub const ALL: SeasonSet = SeasonSet(Self::SPRING | Self::SUMMER | Self::AUTUMN | Self::WINTER);

    pub fn has(self, s: Season) -> bool {
        let bit = match s {
            Season::Spring => Self::SPRING,
            Season::Summer => Self::SUMMER,
            Season::Autumn => Self::AUTUMN,
            Season::Winter => Self::WINTER,
        };
        (self.0 & bit) != 0
    }
}

/// Kinds of access grant. Carries enough data to evaluate `permits`
/// without further world introspection beyond the `Settlement` lookup.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessKind {
    /// Civilian traders / couriers permitted inside a chebyshev disc
    /// around the granter's market_tile.
    MarketCorridor { settlement_id: SettlementId, radius: u8 },
    /// Nomadic band permitted to camp inside a tile-disc on the
    /// granter's territory during certain seasons.
    SeasonalCamp { center: (i32, i32), radius: u8, seasons: SeasonSet },
    /// Civilian transit anywhere, no harvest/build. Optional expiry
    /// tick (`None` = open-ended; v1 NAP-driven grants pass `None`).
    SafePassage { until_tick: Option<u64> },
    /// Full territorial access — alliance semantics.
    FullTerritory,
}

/// One grant row in the table.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct AccessGrant {
    pub kind: AccessKind,
    /// `Some(tick)` for time-limited grants that auto-expire.
    pub expires_tick: Option<u64>,
}

/// Coarse classification of an actor's *intent* for trespass
/// purposes. Drives which `AccessKind` lets them through.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IntruderIntent {
    /// Drafted, raid-party member, or actively attacking — never
    /// permitted regardless of grants.
    Hostile,
    /// `Profession::Trader` carrying tradeable goods — permitted in
    /// `MarketCorridor`.
    CivilianTrader,
    /// Member of a nomadic faction outside its own territory —
    /// permitted in `SeasonalCamp`.
    Nomad,
    /// Any other civilian — permitted by `SafePassage` or `FullTerritory`.
    Civilian,
}

#[derive(Resource, Default)]
pub struct AccessGrantTable {
    pub by_pair: AHashMap<(u32, u32), Vec<AccessGrant>>,
}

impl AccessGrantTable {
    /// Append a grant, deduplicating identical existing entries.
    pub fn insert(&mut self, grantor: u32, grantee: u32, grant: AccessGrant) {
        if grantor == SOLO || grantee == SOLO || grantor == grantee {
            return;
        }
        let v = self.by_pair.entry((grantor, grantee)).or_default();
        if v.iter().any(|g| g.kind == grant.kind) {
            return;
        }
        v.push(grant);
    }

    /// Remove every grant matching `kind`.
    pub fn revoke(&mut self, grantor: u32, grantee: u32, kind: AccessKind) {
        if let Some(v) = self.by_pair.get_mut(&(grantor, grantee)) {
            v.retain(|g| g.kind != kind);
            if v.is_empty() {
                self.by_pair.remove(&(grantor, grantee));
            }
        }
    }

    /// Wipe every grant for the pair. Used on `declare_war`.
    pub fn revoke_all(&mut self, grantor: u32, grantee: u32) {
        self.by_pair.remove(&(grantor, grantee));
    }

    /// Iterate live grants from grantor to grantee.
    pub fn grants(&self, grantor: u32, grantee: u32) -> &[AccessGrant] {
        self.by_pair
            .get(&(grantor, grantee))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Daily GC — drop entries past their `expires_tick`.
    pub fn evict_expired(&mut self, now: u64) {
        for v in self.by_pair.values_mut() {
            v.retain(|g| g.expires_tick.map(|t| t > now).unwrap_or(true));
        }
        self.by_pair.retain(|_, v| !v.is_empty());
    }
}

/// Pure-fn: does any of grantor→grantee's grants permit `intent` at
/// `tile`? `settlements` is a slice of `(settlement_id, market_tile)`
/// resolved by the caller from `SettlementMap` for any
/// `MarketCorridor` grant whose `settlement_id` appears in
/// `grants`. `season` is the current calendar season.
pub fn permits(
    grants: &[AccessGrant],
    intent: IntruderIntent,
    tile: (i32, i32),
    settlements: &[(SettlementId, (i32, i32))],
    season: Season,
) -> bool {
    if matches!(intent, IntruderIntent::Hostile) {
        return false;
    }
    for g in grants {
        match g.kind {
            AccessKind::FullTerritory => return true,
            AccessKind::SafePassage { .. } => {
                if matches!(intent, IntruderIntent::Civilian | IntruderIntent::CivilianTrader | IntruderIntent::Nomad) {
                    return true;
                }
            }
            AccessKind::MarketCorridor { settlement_id, radius } => {
                if !matches!(intent, IntruderIntent::CivilianTrader) {
                    continue;
                }
                let Some((_, mt)) = settlements.iter().find(|(id, _)| *id == settlement_id) else {
                    continue;
                };
                let cheb = (mt.0 - tile.0).abs().max((mt.1 - tile.1).abs());
                if cheb <= radius as i32 {
                    return true;
                }
            }
            AccessKind::SeasonalCamp { center, radius, seasons } => {
                if !matches!(intent, IntruderIntent::Nomad) {
                    continue;
                }
                if !seasons.has(season) {
                    continue;
                }
                let cheb = (center.0 - tile.0).abs().max((center.1 - tile.1).abs());
                if cheb <= radius as i32 {
                    return true;
                }
            }
        }
    }
    false
}

/// Default `MarketCorridor` radius spawned on TradePact accept. Tile
/// scale ≈ 1.5 m → 6 tiles ≈ 9 m (chebyshev). Loose but contained.
pub const DEFAULT_MARKET_CORRIDOR_RADIUS: u8 = 6;

/// Helper: on TradePact accept, spawn one `MarketCorridor` grant per
/// granter settlement → grantee.
pub fn auto_grant_market_corridor(
    table: &mut AccessGrantTable,
    grantor: u32,
    grantee: u32,
    grantor_settlements: &[SettlementId],
) {
    for sid in grantor_settlements {
        table.insert(
            grantor,
            grantee,
            AccessGrant {
                kind: AccessKind::MarketCorridor {
                    settlement_id: *sid,
                    radius: DEFAULT_MARKET_CORRIDOR_RADIUS,
                },
                expires_tick: None,
            },
        );
    }
}

/// Helper: Alliance accept → FullTerritory (mirrors P1 semantics).
pub fn auto_grant_full_territory(table: &mut AccessGrantTable, grantor: u32, grantee: u32) {
    table.insert(
        grantor,
        grantee,
        AccessGrant {
            kind: AccessKind::FullTerritory,
            expires_tick: None,
        },
    );
}

/// Helper: NonAggression accept → SafePassage (open-ended).
pub fn auto_grant_safe_passage(table: &mut AccessGrantTable, grantor: u32, grantee: u32) {
    table.insert(
        grantor,
        grantee,
        AccessGrant {
            kind: AccessKind::SafePassage { until_tick: None },
            expires_tick: None,
        },
    );
}

/// Helper for trespass classification: given an intruder Person's
/// drafting status, raid-party membership, profession, and home
/// faction lifestyle, derive an `IntruderIntent`.
pub fn classify_intent(
    drafted: bool,
    in_raid_party: bool,
    is_trader: bool,
    home_is_mobile: bool,
) -> IntruderIntent {
    if drafted || in_raid_party {
        return IntruderIntent::Hostile;
    }
    if is_trader {
        return IntruderIntent::CivilianTrader;
    }
    if home_is_mobile {
        return IntruderIntent::Nomad;
    }
    IntruderIntent::Civilian
}

/// Convert a `Settlement` query slice into the `(id, market_tile)`
/// slice `permits` consumes. Pure helper.
pub fn settlement_view<'a>(
    settlements: impl Iterator<Item = &'a Settlement>,
) -> Vec<(SettlementId, (i32, i32))> {
    settlements.map(|s| (s.id, s.market_tile)).collect()
}

/// Daily Economy pass. Reconciles `AccessGrantTable` against current
/// treaty state on `DiplomacyLedger` + per-faction settlements:
/// - War on a pair ⇒ wipe both directions (`revoke_all`).
/// - Alliance ⇒ ensure both sides hold `FullTerritory`.
/// - NonAggression (no Alliance) ⇒ ensure `SafePassage` both ways.
/// - TradePact (any treaty state except War) ⇒ ensure `MarketCorridor`
///   per granter settlement, both ways.
///
/// `SeasonalCamp` is player-explicit only (no auto-spawn) and survives
/// reconciliation untouched.
///
/// Bevy system body lives outside this pure-fn module to keep the
/// module's test pool import-free; see `treaty_to_grant_sync_system`
/// below.
pub fn reconcile_pair_grants(
    table: &mut AccessGrantTable,
    a: u32,
    b: u32,
    treaties: crate::simulation::diplomacy::TreatySet,
    a_settlements: &[SettlementId],
    b_settlements: &[SettlementId],
) {
    use crate::simulation::diplomacy::TreatyKind;
    if treaties.has(TreatyKind::War) {
        table.revoke_all(a, b);
        table.revoke_all(b, a);
        return;
    }
    if treaties.has(TreatyKind::Alliance) {
        auto_grant_full_territory(table, a, b);
        auto_grant_full_territory(table, b, a);
    } else if treaties.has(TreatyKind::NonAggression) {
        auto_grant_safe_passage(table, a, b);
        auto_grant_safe_passage(table, b, a);
    } else {
        // Neither alliance nor NAP — revoke any auto-spawned full/safe
        // passage that lingered after a downgrade. Player-explicit
        // SafePassage with non-NAP context is rare; for v1 we treat
        // the auto-spawned variant as derived state and prune it.
        table.revoke(a, b, AccessKind::FullTerritory);
        table.revoke(b, a, AccessKind::FullTerritory);
        table.revoke(a, b, AccessKind::SafePassage { until_tick: None });
        table.revoke(b, a, AccessKind::SafePassage { until_tick: None });
    }
    if treaties.has(TreatyKind::TradePact) {
        auto_grant_market_corridor(table, a, b, a_settlements);
        auto_grant_market_corridor(table, b, a, b_settlements);
    } else {
        // Strip any auto-spawned corridor for settlements either side owns.
        for sid in a_settlements.iter().chain(b_settlements.iter()) {
            table.revoke(
                a,
                b,
                AccessKind::MarketCorridor {
                    settlement_id: *sid,
                    radius: DEFAULT_MARKET_CORRIDOR_RADIUS,
                },
            );
            table.revoke(
                b,
                a,
                AccessKind::MarketCorridor {
                    settlement_id: *sid,
                    radius: DEFAULT_MARKET_CORRIDOR_RADIUS,
                },
            );
        }
    }
}

/// Bevy wrapper around `reconcile_pair_grants`. Runs Economy daily,
/// after `ai_diplomacy_response_system` so accepted-this-tick
/// treaty-form proposals propagate same tick.
pub fn treaty_to_grant_sync_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    ledger: Res<crate::simulation::diplomacy::DiplomacyLedger>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    mut table: ResMut<AccessGrantTable>,
) {
    use crate::world::seasons::TICKS_PER_DAY;
    if clock.tick == 0 || clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    table.evict_expired(clock.tick);
    // Snapshot every pair currently in the ledger.
    let pairs: Vec<(u32, u32, crate::simulation::diplomacy::TreatySet)> = ledger
        .by_pair
        .iter()
        .map(|(p, r)| (p.0, p.1, r.treaties))
        .collect();
    for (a, b, treaties) in pairs {
        let empty: Vec<SettlementId> = Vec::new();
        let a_set: Vec<SettlementId> =
            settlement_map.for_faction(a).iter().copied().collect();
        let b_set: Vec<SettlementId> =
            settlement_map.for_faction(b).iter().copied().collect();
        let a_slice = if a_set.is_empty() { &empty[..] } else { &a_set[..] };
        let b_slice = if b_set.is_empty() { &empty[..] } else { &b_set[..] };
        reconcile_pair_grants(&mut table, a, b, treaties, a_slice, b_slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(kind: AccessKind) -> AccessGrant {
        AccessGrant { kind, expires_tick: None }
    }

    #[test]
    fn full_territory_admits_any_civilian() {
        let grants = vec![mk(AccessKind::FullTerritory)];
        assert!(permits(&grants, IntruderIntent::Civilian, (0, 0), &[], Season::Spring));
        assert!(permits(&grants, IntruderIntent::CivilianTrader, (0, 0), &[], Season::Spring));
        assert!(permits(&grants, IntruderIntent::Nomad, (0, 0), &[], Season::Spring));
    }

    #[test]
    fn full_territory_never_admits_hostile() {
        let grants = vec![mk(AccessKind::FullTerritory)];
        assert!(!permits(&grants, IntruderIntent::Hostile, (0, 0), &[], Season::Spring));
    }

    #[test]
    fn market_corridor_admits_trader_in_radius() {
        let sid = SettlementId(7);
        let grants = vec![mk(AccessKind::MarketCorridor {
            settlement_id: sid,
            radius: 4,
        })];
        let settlements = vec![(sid, (10, 10))];
        assert!(permits(&grants, IntruderIntent::CivilianTrader, (12, 10), &settlements, Season::Spring));
        assert!(permits(&grants, IntruderIntent::CivilianTrader, (14, 10), &settlements, Season::Spring));
        // Outside corridor
        assert!(!permits(&grants, IntruderIntent::CivilianTrader, (20, 10), &settlements, Season::Spring));
        // Wrong intent
        assert!(!permits(&grants, IntruderIntent::Civilian, (12, 10), &settlements, Season::Spring));
    }

    #[test]
    fn seasonal_camp_respects_season_window() {
        let g = mk(AccessKind::SeasonalCamp {
            center: (0, 0),
            radius: 5,
            seasons: SeasonSet(SeasonSet::SUMMER),
        });
        let grants = vec![g];
        assert!(permits(&grants, IntruderIntent::Nomad, (2, 2), &[], Season::Summer));
        assert!(!permits(&grants, IntruderIntent::Nomad, (2, 2), &[], Season::Winter));
    }

    #[test]
    fn safe_passage_admits_civilians_not_hostile() {
        let grants = vec![mk(AccessKind::SafePassage { until_tick: None })];
        assert!(permits(&grants, IntruderIntent::Civilian, (0, 0), &[], Season::Spring));
        assert!(!permits(&grants, IntruderIntent::Hostile, (0, 0), &[], Season::Spring));
    }

    #[test]
    fn revoke_drops_matching_kind() {
        let mut t = AccessGrantTable::default();
        let sid = SettlementId(1);
        t.insert(1, 2, mk(AccessKind::MarketCorridor { settlement_id: sid, radius: 6 }));
        t.insert(1, 2, mk(AccessKind::SafePassage { until_tick: None }));
        assert_eq!(t.grants(1, 2).len(), 2);
        t.revoke(1, 2, AccessKind::MarketCorridor { settlement_id: sid, radius: 6 });
        assert_eq!(t.grants(1, 2).len(), 1);
    }

    #[test]
    fn classify_intent_branches() {
        assert_eq!(classify_intent(true, false, false, false), IntruderIntent::Hostile);
        assert_eq!(classify_intent(false, true, false, false), IntruderIntent::Hostile);
        assert_eq!(classify_intent(false, false, true, false), IntruderIntent::CivilianTrader);
        assert_eq!(classify_intent(false, false, false, true), IntruderIntent::Nomad);
        assert_eq!(classify_intent(false, false, false, false), IntruderIntent::Civilian);
    }

    #[test]
    fn evict_expired_drops_old_grants() {
        let mut t = AccessGrantTable::default();
        t.insert(
            1,
            2,
            AccessGrant {
                kind: AccessKind::SafePassage { until_tick: Some(100) },
                expires_tick: Some(100),
            },
        );
        assert_eq!(t.grants(1, 2).len(), 1);
        t.evict_expired(200);
        assert_eq!(t.grants(1, 2).len(), 0);
    }
}
