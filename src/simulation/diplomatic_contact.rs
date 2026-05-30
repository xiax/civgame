//! Smart-diplomacy P1 — non-omniscient `DiplomaticContactBook`.
//!
//! Every viewer root faction tracks which other root factions it has
//! *evidence* of contact with, plus coarse-band estimates of those
//! partners' population / food stock / military strength derived from
//! observable signals (visited settlements, incident log, scout
//! sightings, gossip carry-through). The AI proposer reads this
//! through `is_known` + `band_*` accessors; it must **never** read the
//! partner's real `FactionStorage` or member count directly.
//!
//! Cadence: `contact_book_update_system` runs in `SimulationSet::Economy`
//! every `TICKS_PER_DAY` ticks. Folds:
//! - `AgentMemory.visited_settlements` ∪ `SettlementMap.by_id` →
//!   `VisitedSettlement` source + market_tile recorded.
//! - Each materialised non-SOLO `FactionData.chief_entity`-anchored
//!   `AgentMemory` from members of *our* root → cross-contact discovery.
//! - `DiplomacyLedger.relation(self, other).incident_log` last 16 entries →
//!   per-incident source bits + freshness.
//! - `SharedKnowledge::Faction(self).by_kind[HostileFactionSighting]` →
//!   `ScoutSighting` source. (Sightings don't attribute to a specific
//!   target faction in v1, so they only flip the `ScoutSighting` bit
//!   for every known partner — a coarse "we're scouting actively"
//!   signal.)
//!
//! Households (`parent_faction.is_some()`) share their village's root
//! and never carry their own contact book.

use crate::collections::AHashMap;
use bevy::prelude::*;

use crate::simulation::diplomacy::{DiplomacyLedger, IncidentKind};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::memory::AgentMemory;
use crate::simulation::person::Person;
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::SettlementMap;
use crate::world::seasons::TICKS_PER_DAY;

/// Cap on `known_market_tiles` per record. Effectively unbounded —
/// nobody visits more than 3–4 markets in v1.
const MAX_KNOWN_MARKET_TILES: usize = 4;

/// Bitset of evidence sources for a contact. ANY non-zero set ⇒ the
/// viewer faction "knows of" the target.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ContactSourceSet(pub u16);

impl ContactSourceSet {
    pub const VISITED_SETTLEMENT: u16 = 1 << 0;
    pub const TRADER_TRIP: u16 = 1 << 1;
    pub const SCOUT_SIGHTING: u16 = 1 << 2;
    pub const GOSSIP_FROM_ALLY: u16 = 1 << 3;
    pub const TRESPASS_ON_US: u16 = 1 << 4;
    pub const INCOMING_PROPOSAL: u16 = 1 << 5;
    pub const MATERIALIZATION: u16 = 1 << 6;
    pub const RAIDED_US: u16 = 1 << 7;
    pub const TRADED_WITH_US: u16 = 1 << 8;
    pub const RECEIVED_AID: u16 = 1 << 9;

    pub fn set(&mut self, bit: u16) {
        self.0 |= bit;
    }
    pub fn has(self, bit: u16) -> bool {
        (self.0 & bit) != 0
    }
    pub fn any(self) -> bool {
        self.0 != 0
    }
}

/// Coarse buckets for partner attributes. AI sees bands, not exact
/// values — the non-omniscience invariant.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PopBand {
    #[default]
    Unknown,
    Low,    // 1..=10
    Medium, // 11..=30
    High,   // 31+
}

impl PopBand {
    pub fn from_count(n: u32) -> Self {
        if n == 0 {
            PopBand::Unknown
        } else if n <= 10 {
            PopBand::Low
        } else if n <= 30 {
            PopBand::Medium
        } else {
            PopBand::High
        }
    }
    /// Coarse numeric for the evaluator (population as f32 scalar).
    pub fn estimate(self) -> f32 {
        match self {
            PopBand::Unknown => 10.0,
            PopBand::Low => 6.0,
            PopBand::Medium => 18.0,
            PopBand::High => 45.0,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum StockBand {
    #[default]
    Unknown,
    Low,
    Medium,
    High,
}

impl StockBand {
    /// Inferred from observed market price: high price ⇒ low stock.
    /// Caller is expected to pass `Some` only when a market sighting
    /// is fresh.
    pub fn from_price(price: f32) -> Self {
        if !price.is_finite() || price <= 0.0 {
            StockBand::Unknown
        } else if price > 1.6 {
            StockBand::Low
        } else if price > 0.9 {
            StockBand::Medium
        } else {
            StockBand::High
        }
    }
    pub fn scarcity_mult(self) -> f32 {
        match self {
            StockBand::Unknown => 1.0,
            StockBand::Low => 1.5,
            StockBand::Medium => 1.0,
            StockBand::High => 0.7,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum MilitaryBand {
    #[default]
    Unknown,
    Low,
    Medium,
    High,
}

impl MilitaryBand {
    /// 0..=N attacks-on-us count: more violence observed ⇒ partner
    /// likely militarised.
    pub fn from_attack_count(c: u32) -> Self {
        if c == 0 {
            MilitaryBand::Unknown
        } else if c <= 2 {
            MilitaryBand::Low
        } else if c <= 6 {
            MilitaryBand::Medium
        } else {
            MilitaryBand::High
        }
    }
    pub fn strength(self) -> f32 {
        match self {
            MilitaryBand::Unknown => 1.0,
            MilitaryBand::Low => 0.6,
            MilitaryBand::Medium => 1.4,
            MilitaryBand::High => 2.2,
        }
    }
}

/// One viewer→target contact record. Sparse — only allocated when at
/// least one source bit gets set.
#[derive(Clone, Debug, Default)]
pub struct ContactRecord {
    pub first_contact_tick: u64,
    pub last_contact_tick: u64,
    pub contact_sources: ContactSourceSet,
    pub known_home_tile: Option<(i32, i32)>,
    pub known_market_tiles: Vec<(i32, i32)>,
    pub last_known_member_count_band: PopBand,
    pub last_known_food_band: StockBand,
    pub last_known_military_band: MilitaryBand,
    pub route_reachable: bool,
}

impl ContactRecord {
    fn record_source(&mut self, source: u16, tick: u64) {
        let was_empty = !self.contact_sources.any();
        self.contact_sources.set(source);
        if was_empty {
            self.first_contact_tick = tick;
        }
        self.last_contact_tick = tick;
    }
    fn record_market(&mut self, market_tile: (i32, i32)) {
        if self.known_market_tiles.iter().any(|t| *t == market_tile) {
            return;
        }
        if self.known_market_tiles.len() >= MAX_KNOWN_MARKET_TILES {
            self.known_market_tiles.remove(0);
        }
        self.known_market_tiles.push(market_tile);
    }
}

/// Per-viewer table of contacts. `last_recomputed_tick` lets the update
/// system skip viewers whose state has been rebuilt this cycle.
#[derive(Clone, Default, Debug)]
pub struct FactionContacts {
    pub known: AHashMap<u32, ContactRecord>,
    pub last_recomputed_tick: u64,
}

#[derive(Resource, Default)]
pub struct DiplomaticContactBook {
    pub by_viewer: AHashMap<u32, FactionContacts>,
}

impl DiplomaticContactBook {
    pub fn is_known(&self, viewer_root: u32, target_root: u32) -> bool {
        if viewer_root == target_root || viewer_root == SOLO || target_root == SOLO {
            return false;
        }
        self.by_viewer
            .get(&viewer_root)
            .and_then(|c| c.known.get(&target_root))
            .map(|r| r.contact_sources.any())
            .unwrap_or(false)
    }
    pub fn contacts_of(&self, viewer_root: u32) -> Option<&FactionContacts> {
        self.by_viewer.get(&viewer_root)
    }
    pub fn record_of(&self, viewer_root: u32, target_root: u32) -> Option<&ContactRecord> {
        self.by_viewer
            .get(&viewer_root)
            .and_then(|c| c.known.get(&target_root))
    }
    fn entry_mut(&mut self, viewer: u32, target: u32) -> &mut ContactRecord {
        self.by_viewer
            .entry(viewer)
            .or_default()
            .known
            .entry(target)
            .or_default()
    }
    /// Public bootstrap used at materialisation / spawn time so the
    /// player faction at least *knows* about its nearby rivals from the
    /// start; otherwise no diplomacy ever fires until first contact.
    pub fn record_materialization(&mut self, viewer: u32, target: u32, tick: u64) {
        if viewer == SOLO || target == SOLO || viewer == target {
            return;
        }
        let rec = self.entry_mut(viewer, target);
        rec.record_source(ContactSourceSet::MATERIALIZATION, tick);
    }
    /// Public hook for the player command path: when the player issues
    /// a proposal toward a known target, the target gains an
    /// `IncomingProposal` contact bit for the reverse direction so the
    /// AI can respond meaningfully.
    pub fn record_incoming_proposal(&mut self, viewer: u32, target: u32, tick: u64) {
        if viewer == SOLO || target == SOLO || viewer == target {
            return;
        }
        let rec = self.entry_mut(viewer, target);
        rec.record_source(ContactSourceSet::INCOMING_PROPOSAL, tick);
    }
}

/// Economy stage, daily. Walks `Person`-bearing `AgentMemory` to fold
/// visited settlements into the contact book; walks the ledger for
/// incident-driven sources; reads `SharedKnowledge::Faction(self)` for
/// scout-sighting bits.
pub fn contact_book_update_system(
    clock: Res<SimClock>,
    mut book: ResMut<DiplomaticContactBook>,
    ledger: Res<DiplomacyLedger>,
    registry: Res<FactionRegistry>,
    settlement_map: Res<SettlementMap>,
    settlements: Query<&crate::simulation::settlement::Settlement>,
    persons: Query<(&AgentMemory, &FactionMember), With<Person>>,
) {
    let now = clock.tick;
    if now == 0 || now % TICKS_PER_DAY as u64 != 0 {
        return;
    }

    // ── Pass 1: settlement-visit-driven contacts. ────────────────────
    // For each Person, derive the viewer's root faction and walk the
    // member's `visited_settlements` slot ring; for each settlement,
    // resolve owner faction and stamp `VisitedSettlement` on both sides
    // (we know them; they know us by being visited).
    for (memory, member) in persons.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        let viewer_root = registry.root_faction(member.faction_id);
        for (sid, _fresh) in memory.known_settlements() {
            let Some(entity) = settlement_map.by_id.get(&sid).copied() else {
                continue;
            };
            let Ok(s) = settlements.get(entity) else {
                continue;
            };
            let target_root = registry.root_faction(s.owner_faction);
            if target_root == viewer_root || target_root == SOLO {
                continue;
            }
            {
                let rec = book.entry_mut(viewer_root, target_root);
                rec.record_source(ContactSourceSet::VISITED_SETTLEMENT, now);
                rec.record_market(s.market_tile);
                if rec.known_home_tile.is_none() {
                    if let Some(d) = registry.factions.get(&target_root) {
                        rec.known_home_tile = Some(d.home_tile);
                    }
                }
                rec.route_reachable = true;
            }
            // Reciprocal — the target faction has "had visitors from us."
            // Treat as a TraderTrip-style soft signal.
            let rec = book.entry_mut(target_root, viewer_root);
            rec.record_source(ContactSourceSet::TRADER_TRIP, now);
        }
    }

    // ── Pass 2: ledger-incident-driven contacts. ─────────────────────
    // Walk every pair with a non-empty incident log; each incident
    // flips a source bit + updates the matching band.
    let pair_snapshots: Vec<(u32, u32, Vec<IncidentKind>)> = ledger
        .by_pair
        .iter()
        .map(|(pair, rel)| {
            let incidents: Vec<IncidentKind> =
                rel.incident_log.iter().map(|i| i.kind.clone()).collect();
            (pair.0, pair.1, incidents)
        })
        .collect();
    for (a, b, incidents) in pair_snapshots {
        if a == SOLO || b == SOLO {
            continue;
        }
        let root_a = registry.root_faction(a);
        let root_b = registry.root_faction(b);
        if root_a == root_b {
            continue;
        }
        let mut attack_count_a_on_b: u32 = 0;
        let mut attack_count_b_on_a: u32 = 0;
        let mut traded_units: u32 = 0;
        let mut aided_units: u32 = 0;
        for inc in &incidents {
            match inc {
                IncidentKind::Raid { .. } => {
                    // Ledger ordering doesn't tell us direction
                    // canonically; record on both sides so each gains
                    // the RAIDED_US bit (cheap, no-op when already set).
                    let ra = book.entry_mut(root_a, root_b);
                    ra.record_source(ContactSourceSet::RAIDED_US, 0);
                    let rb = book.entry_mut(root_b, root_a);
                    rb.record_source(ContactSourceSet::RAIDED_US, 0);
                    attack_count_a_on_b = attack_count_a_on_b.saturating_add(1);
                    attack_count_b_on_a = attack_count_b_on_a.saturating_add(1);
                }
                IncidentKind::Attack { aggressor, victim_count } => {
                    if *aggressor == a {
                        attack_count_a_on_b = attack_count_a_on_b.saturating_add(*victim_count as u32);
                    } else if *aggressor == b {
                        attack_count_b_on_a = attack_count_b_on_a.saturating_add(*victim_count as u32);
                    }
                }
                IncidentKind::Trespass { .. } | IncidentKind::IgnoredWarning => {
                    let ra = book.entry_mut(root_a, root_b);
                    ra.record_source(ContactSourceSet::TRESPASS_ON_US, 0);
                    let rb = book.entry_mut(root_b, root_a);
                    rb.record_source(ContactSourceSet::TRESPASS_ON_US, 0);
                }
                IncidentKind::TradeCompleted { value_currency } => {
                    let ra = book.entry_mut(root_a, root_b);
                    ra.record_source(ContactSourceSet::TRADED_WITH_US, 0);
                    let rb = book.entry_mut(root_b, root_a);
                    rb.record_source(ContactSourceSet::TRADED_WITH_US, 0);
                    traded_units = traded_units.saturating_add(*value_currency);
                }
                IncidentKind::Aid { resource_units } => {
                    let ra = book.entry_mut(root_a, root_b);
                    ra.record_source(ContactSourceSet::RECEIVED_AID, 0);
                    let rb = book.entry_mut(root_b, root_a);
                    rb.record_source(ContactSourceSet::RECEIVED_AID, 0);
                    aided_units = aided_units.saturating_add(*resource_units);
                }
                IncidentKind::TreatyFormed(_)
                | IncidentKind::TreatyBroken(_)
                | IncidentKind::SharedEnemy { .. }
                | IncidentKind::TributeAccepted
                | IncidentKind::DealAccepted { .. }
                | IncidentKind::DealDelivered { .. }
                | IncidentKind::DealDefaulted { .. } => {
                    // Treaty moves and deal milestones imply prior
                    // contact already recorded.
                }
            }
        }
        // Military band: each side reads its own "attacks taken from
        // the other." Higher = more militarised partner.
        if attack_count_b_on_a > 0 {
            let ra = book.entry_mut(root_a, root_b);
            ra.last_known_military_band = MilitaryBand::from_attack_count(attack_count_b_on_a);
        }
        if attack_count_a_on_b > 0 {
            let rb = book.entry_mut(root_b, root_a);
            rb.last_known_military_band = MilitaryBand::from_attack_count(attack_count_a_on_b);
        }
        // Food / population bands: only refined when there's been
        // *trade* or *aid* — those are the signals that leak stock
        // info. Default stays Unknown.
        if traded_units > 0 {
            // Lots of trade ⇒ partner has surplus to sell ⇒ medium-high stock.
            let band = if traded_units > 100 {
                StockBand::High
            } else {
                StockBand::Medium
            };
            let ra = book.entry_mut(root_a, root_b);
            ra.last_known_food_band = band;
            let rb = book.entry_mut(root_b, root_a);
            rb.last_known_food_band = band;
        }
        if aided_units > 0 {
            // We received aid ⇒ they had surplus to give.
            let ra = book.entry_mut(root_a, root_b);
            ra.last_known_food_band = StockBand::High;
        }
    }

    // ── Pass 3: refresh population bands for materialised partners. ──
    // Population band derived from the partner's last visited
    // settlement(s)' peak_population if we've ever visited. This is the
    // single observable population signal the AI is allowed to use.
    let viewers: Vec<u32> = book.by_viewer.keys().copied().collect();
    for viewer in viewers {
        let target_ids: Vec<u32> = book
            .by_viewer
            .get(&viewer)
            .map(|c| c.known.keys().copied().collect())
            .unwrap_or_default();
        for target in target_ids {
            // Walk every settlement the viewer has visited that the
            // partner owns. peak_population per settlement gives a
            // band-level estimate.
            let mut max_pop: u32 = 0;
            for sid in settlement_map.for_faction(target) {
                let Some(entity) = settlement_map.by_id.get(sid).copied() else {
                    continue;
                };
                let Ok(s) = settlements.get(entity) else {
                    continue;
                };
                // Was this settlement on the viewer's `known_market_tiles`?
                let known = book
                    .by_viewer
                    .get(&viewer)
                    .and_then(|c| c.known.get(&target))
                    .map(|r| r.known_market_tiles.iter().any(|t| *t == s.market_tile))
                    .unwrap_or(false);
                if known {
                    max_pop = max_pop.max(s.peak_population);
                }
            }
            if max_pop > 0 {
                let rec = book.entry_mut(viewer, target);
                rec.last_known_member_count_band = PopBand::from_count(max_pop);
            }
        }
        if let Some(c) = book.by_viewer.get_mut(&viewer) {
            c.last_recomputed_tick = now;
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_pair_returns_false() {
        let book = DiplomaticContactBook::default();
        assert!(!book.is_known(1, 2));
    }

    #[test]
    fn record_materialization_marks_known() {
        let mut book = DiplomaticContactBook::default();
        book.record_materialization(1, 2, 100);
        assert!(book.is_known(1, 2));
        assert!(!book.is_known(2, 1), "reverse direction not auto-set");
    }

    #[test]
    fn self_or_solo_never_known() {
        let mut book = DiplomaticContactBook::default();
        book.record_materialization(1, 1, 0);
        book.record_materialization(SOLO, 2, 0);
        book.record_materialization(1, SOLO, 0);
        assert!(!book.is_known(1, 1));
        assert!(!book.is_known(SOLO, 2));
        assert!(!book.is_known(1, SOLO));
    }

    #[test]
    fn pop_band_buckets_correctly() {
        assert_eq!(PopBand::from_count(0), PopBand::Unknown);
        assert_eq!(PopBand::from_count(8), PopBand::Low);
        assert_eq!(PopBand::from_count(20), PopBand::Medium);
        assert_eq!(PopBand::from_count(80), PopBand::High);
    }

    #[test]
    fn stock_band_from_price_inverts() {
        assert_eq!(StockBand::from_price(2.0), StockBand::Low); // expensive
        assert_eq!(StockBand::from_price(1.0), StockBand::Medium);
        assert_eq!(StockBand::from_price(0.5), StockBand::High); // cheap
    }

    #[test]
    fn military_band_from_attacks() {
        assert_eq!(MilitaryBand::from_attack_count(0), MilitaryBand::Unknown);
        assert_eq!(MilitaryBand::from_attack_count(1), MilitaryBand::Low);
        assert_eq!(MilitaryBand::from_attack_count(4), MilitaryBand::Medium);
        assert_eq!(MilitaryBand::from_attack_count(10), MilitaryBand::High);
    }
}
