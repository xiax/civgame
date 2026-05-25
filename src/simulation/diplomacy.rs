//! Faction-pair diplomacy ledger: treaties + reputation + incident log +
//! proposal lifecycle. War is exclusive; alliance / trade / non-aggression
//! coexist.
//!
//! Composes with — does not replace — the existing tributary axis
//! (`FactionData.{dominance_over, subordinate_to}`).
//!
//! Cadence:
//! - `reputation_decay_system` (Economy, daily) applies half-life decay.
//! - `proposal_expiry_system` (Economy, daily) reaps stale proposals.
//! - `ai_diplomacy_response_system` (Economy, TICKS_PER_DAY/4) drains
//!   AI-faction inboxes via the pure-fn `evaluate_proposal`.
//! - `ai_diplomacy_proposal_system` (Economy, TICKS_PER_DAY) generates
//!   AI-initiated proposals (offset by `faction_id % TICKS_PER_DAY`).

use ahash::AHashMap;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::simulation::faction::{FactionRegistry, SOLO};
use crate::simulation::schedule::SimClock;
use crate::world::seasons::TICKS_PER_DAY;

// ── Constants ────────────────────────────────────────────────────────────

/// Reputation track caps.
pub const TRUST_MIN: i16 = -100;
pub const TRUST_MAX: i16 = 100;
pub const FEAR_MAX: i16 = 100;
pub const GRIEVANCE_MAX: i16 = 100;
pub const FAMILIARITY_MAX: u16 = 10_000;

/// Per-day half-life multipliers, applied each daily Economy tick.
/// Computed once: `decayed = old * mul`, integer-rounded. `mul = 0.5 ^ (1 / half_life_days)`.
///
/// - Trust: 90-day half-life → mul ≈ 0.9924
/// - Fear:  10-day half-life → mul ≈ 0.9330
/// - Grievance: 365-day half-life → mul ≈ 0.9981
pub const TRUST_DECAY_PER_DAY: f32 = 0.9924;
pub const FEAR_DECAY_PER_DAY: f32 = 0.9330;
pub const GRIEVANCE_DECAY_PER_DAY: f32 = 0.9981;

/// Proposal expiry — one game-week.
pub const PROPOSAL_EXPIRY_TICKS: u64 = TICKS_PER_DAY as u64 * 7;

/// Per-relation incident log ring length.
pub const INCIDENT_LOG_LEN: usize = 16;

/// AI accept/reject thresholds for `evaluate_proposal`.
pub const TRUST_ACCEPT_ALLIANCE: i16 = 40;
pub const TRUST_ACCEPT_TRADE: i16 = 0;
pub const GRIEVANCE_BLOCK_TRADE: i16 = 40;
pub const FEAR_ACCEPT_PEACE: i16 = 60;
pub const FEAR_ACCEPT_TRIBUTE: i16 = 80;
pub const FAMILIARITY_ALLIANCE_GATE: u16 = 200;

/// Reputation deltas applied by `record_incident`.
pub const GRIEVANCE_RAID: i16 = 30;
pub const GRIEVANCE_TRESPASS_REPEAT: i16 = 2;
pub const GRIEVANCE_IGNORED_WARNING: i16 = 5;
pub const FEAR_ATTACK_PER_VICTIM: i16 = 4;
pub const TRUST_TRADE_PER_UNIT: f32 = 0.05;
pub const TRUST_AID_PER_UNIT: f32 = 0.2;
pub const TRUST_SHARED_ENEMY: i16 = 5;
pub const FAMILIARITY_PER_INCIDENT: u16 = 4;
pub const FAMILIARITY_PER_TRADE: u16 = 12;

// ── Types ────────────────────────────────────────────────────────────────

/// Canonical (min, max) faction pair for ledger keying. Always ordered.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct FactionPair(pub u32, pub u32);

impl FactionPair {
    pub fn new(a: u32, b: u32) -> Self {
        if a <= b {
            FactionPair(a, b)
        } else {
            FactionPair(b, a)
        }
    }
    pub fn other(&self, faction_id: u32) -> Option<u32> {
        if self.0 == faction_id {
            Some(self.1)
        } else if self.1 == faction_id {
            Some(self.0)
        } else {
            None
        }
    }
    pub fn contains(&self, faction_id: u32) -> bool {
        self.0 == faction_id || self.1 == faction_id
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TreatyKind {
    TradePact,
    Alliance,
    NonAggression,
    War,
}

/// Bitset of active treaties between a pair.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreatySet(pub u8);

impl TreatySet {
    const TRADE: u8 = 1 << 0;
    const ALLIANCE: u8 = 1 << 1;
    const NON_AGGRESSION: u8 = 1 << 2;
    const WAR: u8 = 1 << 3;

    pub fn has(&self, kind: TreatyKind) -> bool {
        let bit = match kind {
            TreatyKind::TradePact => Self::TRADE,
            TreatyKind::Alliance => Self::ALLIANCE,
            TreatyKind::NonAggression => Self::NON_AGGRESSION,
            TreatyKind::War => Self::WAR,
        };
        (self.0 & bit) != 0
    }
    pub fn insert(&mut self, kind: TreatyKind) {
        self.0 |= match kind {
            TreatyKind::TradePact => Self::TRADE,
            TreatyKind::Alliance => Self::ALLIANCE,
            TreatyKind::NonAggression => Self::NON_AGGRESSION,
            TreatyKind::War => Self::WAR,
        };
    }
    pub fn remove(&mut self, kind: TreatyKind) {
        self.0 &= !match kind {
            TreatyKind::TradePact => Self::TRADE,
            TreatyKind::Alliance => Self::ALLIANCE,
            TreatyKind::NonAggression => Self::NON_AGGRESSION,
            TreatyKind::War => Self::WAR,
        };
    }
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

/// Round-to-nearest decay step. With multiplier 0.9981 and value 80,
/// produces 80 (no change) — slow decays don't bleed a unit per day.
/// Tied to sign so negative trust decays toward 0 from below too.
#[inline]
fn decay_round(value: i16, mul: f32) -> i16 {
    // Round-to-nearest. At small values (|v| ≤ ~7 for fear,
    // ~250 for grievance) the value sticks; reputation drift to
    // exactly zero relies on fresh incidents resetting the value
    // — an accepted ledger quirk.
    ((value as f32) * mul).round() as i16
}

#[derive(Copy, Clone, Default, Debug, Serialize, Deserialize)]
pub struct Reputation {
    pub trust: i16,        // -100..+100
    pub fear: i16,         //  0..+100
    pub grievance: i16,    //  0..+100
    pub familiarity: u16,  //  0..u16::MAX (capped at FAMILIARITY_MAX)
}

impl Reputation {
    pub fn clamp(&mut self) {
        self.trust = self.trust.clamp(TRUST_MIN, TRUST_MAX);
        self.fear = self.fear.clamp(0, FEAR_MAX);
        self.grievance = self.grievance.clamp(0, GRIEVANCE_MAX);
        if self.familiarity > FAMILIARITY_MAX {
            self.familiarity = FAMILIARITY_MAX;
        }
    }

    /// Apply one daily-tick of half-life decay to all tracks. Uses
    /// round-to-nearest so slow-decay tracks (Grievance) don't bleed
    /// off one unit per day from integer truncation alone.
    pub fn decay_one_day(&mut self) {
        self.trust = decay_round(self.trust, TRUST_DECAY_PER_DAY);
        self.fear = decay_round(self.fear, FEAR_DECAY_PER_DAY);
        self.grievance = decay_round(self.grievance, GRIEVANCE_DECAY_PER_DAY);
        // Familiarity does not decay — it's a "have we met" counter.
    }

    /// One-word attitude derived from reputation + treaty axis. UI label.
    pub fn attitude_label(&self, treaties: TreatySet) -> &'static str {
        if treaties.has(TreatyKind::War) {
            "Hostile"
        } else if treaties.has(TreatyKind::Alliance) {
            "Allied"
        } else if self.grievance > 60 {
            "Resentful"
        } else if self.trust > 40 {
            "Friendly"
        } else if self.fear > 60 {
            "Wary"
        } else if treaties.has(TreatyKind::TradePact) {
            "Trading"
        } else {
            "Neutral"
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum IncidentKind {
    Trespass { tile: (i32, i32), warned: bool },
    IgnoredWarning,
    Attack { aggressor: u32, victim_count: u16 },
    Raid { stolen_food: u32 },
    TradeCompleted { value_currency: u32 },
    Aid { resource_units: u32 },
    SharedEnemy { common_target: u32 },
    TreatyFormed(TreatyKind),
    TreatyBroken(TreatyKind),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Incident {
    pub tick: u64,
    pub kind: IncidentKind,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiplomaticRelation {
    pub treaties: TreatySet,
    pub reputation: Reputation,
    pub last_contact_tick: u64,
    pub incident_log: VecDeque<Incident>,
}

impl DiplomaticRelation {
    fn push_incident(&mut self, tick: u64, kind: IncidentKind) {
        self.last_contact_tick = tick;
        if self.incident_log.len() >= INCIDENT_LOG_LEN {
            self.incident_log.pop_front();
        }
        self.incident_log.push_back(Incident { tick, kind });
    }
}

// ── Proposals ────────────────────────────────────────────────────────────

/// Monotonic per-process proposal id. `0` reserved for "not yet allocated".
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct ProposalId(pub u64);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiplomacyProposal {
    OfferTradePact,
    OfferAlliance,
    OfferPeace,
    OfferNonAggression,
    DemandTribute,
    OfferAid {
        resource_id: u16,
        qty: u32,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalResponse {
    Accept,
    Reject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingProposal {
    pub id: ProposalId,
    pub from_faction: u32,
    pub to_faction: u32,
    pub proposal: DiplomacyProposal,
    pub posted_tick: u64,
}

// ── Ledger ───────────────────────────────────────────────────────────────

#[derive(Resource, Default)]
pub struct DiplomacyLedger {
    pub by_pair: AHashMap<FactionPair, DiplomaticRelation>,
    pub proposals: AHashMap<ProposalId, PendingProposal>,
    pub inbox_by_faction: AHashMap<u32, Vec<ProposalId>>,
    pub next_proposal_id: u64,
}

impl DiplomacyLedger {
    pub fn relation(&self, a: u32, b: u32) -> Option<&DiplomaticRelation> {
        self.by_pair.get(&FactionPair::new(a, b))
    }
    pub fn relation_mut(&mut self, a: u32, b: u32) -> &mut DiplomaticRelation {
        self.by_pair.entry(FactionPair::new(a, b)).or_default()
    }
    pub fn treaties(&self, a: u32, b: u32) -> TreatySet {
        self.relation(a, b).map(|r| r.treaties).unwrap_or_default()
    }
    pub fn has_treaty(&self, a: u32, b: u32, kind: TreatyKind) -> bool {
        self.treaties(a, b).has(kind)
    }

    pub fn alloc_proposal_id(&mut self) -> ProposalId {
        self.next_proposal_id += 1;
        ProposalId(self.next_proposal_id)
    }

    /// Drain incoming proposals targeted at `faction_id` (consumes the
    /// inbox for that faction). Returns the proposal-ids in posted order.
    pub fn drain_inbox(&mut self, faction_id: u32) -> Vec<ProposalId> {
        self.inbox_by_faction
            .remove(&faction_id)
            .unwrap_or_default()
    }

    /// Post a new proposal; pushes the id onto the recipient's inbox.
    pub fn post_proposal(
        &mut self,
        from: u32,
        to: u32,
        proposal: DiplomacyProposal,
        tick: u64,
    ) -> ProposalId {
        let id = self.alloc_proposal_id();
        self.proposals.insert(
            id,
            PendingProposal {
                id,
                from_faction: from,
                to_faction: to,
                proposal,
                posted_tick: tick,
            },
        );
        self.inbox_by_faction.entry(to).or_default().push(id);
        id
    }

    /// Remove a proposal by id (consumed on Accept / Reject / Expire).
    /// Also strips it from the inbox.
    pub fn consume_proposal(&mut self, id: ProposalId) -> Option<PendingProposal> {
        let p = self.proposals.remove(&id)?;
        if let Some(inbox) = self.inbox_by_faction.get_mut(&p.to_faction) {
            inbox.retain(|x| *x != id);
        }
        Some(p)
    }
}

// ── Treaty ops ───────────────────────────────────────────────────────────

/// Apply `War` between (a, b). Cancels every coexistence treaty and
/// emits matching `TreatyBroken` incidents. Idempotent.
pub fn declare_war(ledger: &mut DiplomacyLedger, a: u32, b: u32, tick: u64) {
    let r = ledger.relation_mut(a, b);
    let was_at_war = r.treaties.has(TreatyKind::War);
    for kind in [
        TreatyKind::TradePact,
        TreatyKind::Alliance,
        TreatyKind::NonAggression,
    ] {
        if r.treaties.has(kind) {
            r.treaties.remove(kind);
            r.push_incident(tick, IncidentKind::TreatyBroken(kind));
        }
    }
    if !was_at_war {
        r.treaties.insert(TreatyKind::War);
        r.push_incident(tick, IncidentKind::TreatyFormed(TreatyKind::War));
    }
}

/// Apply a non-war treaty between (a, b). Rejects if currently at war
/// (caller must clear war via `OfferPeace`). Idempotent.
pub fn form_treaty(
    ledger: &mut DiplomacyLedger,
    a: u32,
    b: u32,
    kind: TreatyKind,
    tick: u64,
) -> bool {
    if kind == TreatyKind::War {
        declare_war(ledger, a, b, tick);
        return true;
    }
    let r = ledger.relation_mut(a, b);
    if r.treaties.has(TreatyKind::War) {
        return false;
    }
    if r.treaties.has(kind) {
        return true;
    }
    r.treaties.insert(kind);
    r.push_incident(tick, IncidentKind::TreatyFormed(kind));
    true
}

/// Tear down one treaty. War clears via `OfferPeace`'s accept path
/// (which calls this with `War`). Idempotent.
pub fn break_treaty(ledger: &mut DiplomacyLedger, a: u32, b: u32, kind: TreatyKind, tick: u64) {
    let r = ledger.relation_mut(a, b);
    if !r.treaties.has(kind) {
        return;
    }
    r.treaties.remove(kind);
    r.push_incident(tick, IncidentKind::TreatyBroken(kind));
}

// ── Incident -> reputation deltas ────────────────────────────────────────

pub fn record_incident(ledger: &mut DiplomacyLedger, a: u32, b: u32, tick: u64, kind: IncidentKind) {
    let r = ledger.relation_mut(a, b);
    match &kind {
        IncidentKind::Trespass { warned, .. } => {
            if !warned {
                // First crossing — warning emitted, no rep change.
            } else {
                r.reputation.grievance =
                    r.reputation.grievance.saturating_add(GRIEVANCE_TRESPASS_REPEAT);
            }
            r.reputation.familiarity = r.reputation.familiarity.saturating_add(FAMILIARITY_PER_INCIDENT);
        }
        IncidentKind::IgnoredWarning => {
            r.reputation.grievance = r.reputation.grievance.saturating_add(GRIEVANCE_IGNORED_WARNING);
        }
        IncidentKind::Attack { victim_count, .. } => {
            r.reputation.fear =
                r.reputation.fear.saturating_add(FEAR_ATTACK_PER_VICTIM.saturating_mul(*victim_count as i16));
            r.reputation.grievance = r.reputation.grievance.saturating_add(GRIEVANCE_RAID / 2);
            r.reputation.trust = r.reputation.trust.saturating_sub(10);
        }
        IncidentKind::Raid { .. } => {
            r.reputation.grievance = r.reputation.grievance.saturating_add(GRIEVANCE_RAID);
            r.reputation.trust = r.reputation.trust.saturating_sub(20);
            r.reputation.fear = r.reputation.fear.saturating_add(15);
        }
        IncidentKind::TradeCompleted { value_currency } => {
            let bump = (TRUST_TRADE_PER_UNIT * *value_currency as f32).round() as i16;
            r.reputation.trust = r.reputation.trust.saturating_add(bump);
            r.reputation.familiarity = r.reputation.familiarity.saturating_add(FAMILIARITY_PER_TRADE);
        }
        IncidentKind::Aid { resource_units } => {
            let bump = (TRUST_AID_PER_UNIT * *resource_units as f32).round() as i16;
            r.reputation.trust = r.reputation.trust.saturating_add(bump);
        }
        IncidentKind::SharedEnemy { .. } => {
            r.reputation.trust = r.reputation.trust.saturating_add(TRUST_SHARED_ENEMY);
        }
        IncidentKind::TreatyFormed(_) | IncidentKind::TreatyBroken(_) => {
            // No automatic rep delta — caller decides (war is exclusive).
        }
    }
    r.reputation.clamp();
    r.push_incident(tick, kind);
}

// ── AI evaluator ─────────────────────────────────────────────────────────

/// Pure-fn AI policy. Returns `Accept` or `Reject` for a proposal given
/// the receiver's current relation. Caller (system) applies the side
/// effects.
pub fn evaluate_proposal(
    proposal: DiplomacyProposal,
    relation: &DiplomaticRelation,
) -> ProposalResponse {
    let rep = &relation.reputation;
    let treaties = relation.treaties;
    match proposal {
        DiplomacyProposal::OfferPeace => {
            if treaties.has(TreatyKind::War)
                && (rep.fear >= FEAR_ACCEPT_PEACE || rep.grievance < 30)
            {
                ProposalResponse::Accept
            } else if !treaties.has(TreatyKind::War) {
                ProposalResponse::Accept // already at peace; idempotent accept
            } else {
                ProposalResponse::Reject
            }
        }
        DiplomacyProposal::OfferAlliance => {
            if treaties.has(TreatyKind::War) || treaties.has(TreatyKind::Alliance) {
                return if treaties.has(TreatyKind::Alliance) {
                    ProposalResponse::Accept
                } else {
                    ProposalResponse::Reject
                };
            }
            if rep.trust >= TRUST_ACCEPT_ALLIANCE
                && rep.familiarity >= FAMILIARITY_ALLIANCE_GATE
                && rep.grievance < 40
            {
                ProposalResponse::Accept
            } else {
                ProposalResponse::Reject
            }
        }
        DiplomacyProposal::OfferTradePact => {
            if treaties.has(TreatyKind::War) {
                return ProposalResponse::Reject;
            }
            if treaties.has(TreatyKind::TradePact) {
                return ProposalResponse::Accept;
            }
            if rep.trust >= TRUST_ACCEPT_TRADE && rep.grievance < GRIEVANCE_BLOCK_TRADE {
                ProposalResponse::Accept
            } else {
                ProposalResponse::Reject
            }
        }
        DiplomacyProposal::OfferNonAggression => {
            if treaties.has(TreatyKind::War) {
                return ProposalResponse::Reject;
            }
            if rep.grievance < 60 {
                ProposalResponse::Accept
            } else {
                ProposalResponse::Reject
            }
        }
        DiplomacyProposal::DemandTribute => {
            // Only accept under duress.
            if rep.fear >= FEAR_ACCEPT_TRIBUTE {
                ProposalResponse::Accept
            } else {
                ProposalResponse::Reject
            }
        }
        DiplomacyProposal::OfferAid { .. } => {
            // Aid is always accepted unless at war (in which case suspicious).
            if treaties.has(TreatyKind::War) {
                ProposalResponse::Reject
            } else {
                ProposalResponse::Accept
            }
        }
    }
}

// ── Systems ──────────────────────────────────────────────────────────────

/// Daily Economy decay over every relation.
pub fn reputation_decay_system(clock: Res<SimClock>, mut ledger: ResMut<DiplomacyLedger>) {
    if clock.tick == 0 || clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    for relation in ledger.by_pair.values_mut() {
        relation.reputation.decay_one_day();
    }
}

/// Daily Economy expiry pass. Drops proposals older than
/// `PROPOSAL_EXPIRY_TICKS`.
pub fn proposal_expiry_system(clock: Res<SimClock>, mut ledger: ResMut<DiplomacyLedger>) {
    if clock.tick == 0 || clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick;
    let expired: Vec<ProposalId> = ledger
        .proposals
        .iter()
        .filter(|(_, p)| now.saturating_sub(p.posted_tick) >= PROPOSAL_EXPIRY_TICKS)
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        ledger.consume_proposal(id);
    }
}

/// Quarter-daily Economy pass. For every uncontrolled (AI) faction with
/// pending proposals, drain the inbox and apply `evaluate_proposal`.
pub fn ai_diplomacy_response_system(
    clock: Res<SimClock>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    mut ledger: ResMut<DiplomacyLedger>,
) {
    if clock.tick == 0 || clock.tick % (TICKS_PER_DAY as u64 / 4) != 0 {
        return;
    }
    // Snapshot faction ids with non-empty inboxes — borrowing-safe.
    let factions: Vec<u32> = ledger
        .inbox_by_faction
        .iter()
        .filter(|(fid, ids)| !controlled.contains(**fid) && !ids.is_empty())
        .map(|(fid, _)| *fid)
        .collect();
    let now = clock.tick;
    for fid in factions {
        let ids = ledger.drain_inbox(fid);
        for pid in ids {
            let Some(p) = ledger.consume_proposal(pid) else {
                continue;
            };
            let response = {
                let relation = ledger
                    .by_pair
                    .get(&FactionPair::new(p.from_faction, p.to_faction))
                    .cloned()
                    .unwrap_or_default();
                evaluate_proposal(p.proposal, &relation)
            };
            if response == ProposalResponse::Accept {
                apply_accepted_proposal(&mut ledger, p.from_faction, p.to_faction, p.proposal, now);
            }
            // Reject is a no-op on the ledger; future: emit a log entry.
        }
    }
}

/// Daily Economy pass: for every materialised non-SOLO AI faction with
/// a meaningful diplomatic opportunity, emit one proposal. Heavily
/// throttled — most factions emit nothing on most ticks.
pub fn ai_diplomacy_proposal_system(
    clock: Res<SimClock>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    registry: Res<FactionRegistry>,
    mut ledger: ResMut<DiplomacyLedger>,
) {
    let now = clock.tick;
    if now == 0 || now % (TICKS_PER_DAY as u64 / 4) != 0 {
        return;
    }
    // Snapshot AI factions (skip player-controlled, SOLO, household
    // sub-factions). Households share root with their village.
    let candidates: Vec<u32> = registry
        .factions
        .iter()
        .filter(|(fid, data)| {
            **fid != SOLO
                && !controlled.contains(**fid)
                && data.parent_faction.is_none()
                && data.materialized
        })
        .map(|(fid, _)| *fid)
        .collect();
    if candidates.is_empty() {
        return;
    }
    let pair_candidates: Vec<u32> = registry
        .factions
        .iter()
        .filter(|(fid, data)| **fid != SOLO && data.parent_faction.is_none())
        .map(|(fid, _)| *fid)
        .collect();
    for from in candidates {
        // Cheap per-faction-per-day cadence: only fire when faction id
        // matches the day-aligned offset.
        let day = now / TICKS_PER_DAY as u64;
        if (day + from as u64) % 5 != 0 {
            continue;
        }
        for to in &pair_candidates {
            if *to == from {
                continue;
            }
            // Don't propose to a faction we share root with.
            if registry.root_faction(from) == registry.root_faction(*to) {
                continue;
            }
            if ledger.has_treaty(from, *to, TreatyKind::War) {
                // Maybe offer peace if we're tired.
                let relation = ledger.relation(from, *to).cloned().unwrap_or_default();
                if relation.reputation.fear >= FEAR_ACCEPT_PEACE
                    || relation.reputation.grievance < 20
                {
                    ledger.post_proposal(from, *to, DiplomacyProposal::OfferPeace, now);
                }
                continue;
            }
            let relation = ledger.relation(from, *to).cloned().unwrap_or_default();
            let rep = relation.reputation;
            // Alliance: very high trust + familiar.
            if rep.trust >= TRUST_ACCEPT_ALLIANCE + 10
                && rep.familiarity >= FAMILIARITY_ALLIANCE_GATE
                && !ledger.has_treaty(from, *to, TreatyKind::Alliance)
            {
                ledger.post_proposal(from, *to, DiplomacyProposal::OfferAlliance, now);
            } else if rep.trust >= TRUST_ACCEPT_TRADE
                && rep.grievance < GRIEVANCE_BLOCK_TRADE
                && !ledger.has_treaty(from, *to, TreatyKind::TradePact)
            {
                ledger.post_proposal(from, *to, DiplomacyProposal::OfferTradePact, now);
            }
        }
    }
}

/// Apply an Accept side-effect for any proposal kind. Public so the
/// player-command dispatcher can reuse on `RespondDiplomacyProposal`.
pub fn apply_accepted_proposal(
    ledger: &mut DiplomacyLedger,
    from: u32,
    to: u32,
    proposal: DiplomacyProposal,
    tick: u64,
) {
    match proposal {
        DiplomacyProposal::OfferPeace => {
            break_treaty(ledger, from, to, TreatyKind::War, tick);
            form_treaty(ledger, from, to, TreatyKind::NonAggression, tick);
        }
        DiplomacyProposal::OfferTradePact => {
            form_treaty(ledger, from, to, TreatyKind::TradePact, tick);
        }
        DiplomacyProposal::OfferAlliance => {
            form_treaty(ledger, from, to, TreatyKind::Alliance, tick);
        }
        DiplomacyProposal::OfferNonAggression => {
            form_treaty(ledger, from, to, TreatyKind::NonAggression, tick);
        }
        DiplomacyProposal::DemandTribute => {
            // No FactionData mutation yet — tribute side-effect deferred to
            // the existing `dominance_over` / `subordinate_to` axis.
            // Record familiarity bump only.
            let r = ledger.relation_mut(from, to);
            r.reputation.familiarity = r.reputation.familiarity.saturating_add(FAMILIARITY_PER_INCIDENT);
        }
        DiplomacyProposal::OfferAid { qty, .. } => {
            record_incident(
                ledger,
                from,
                to,
                tick,
                IncidentKind::Aid { resource_units: qty },
            );
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faction_pair_is_canonical() {
        assert_eq!(FactionPair::new(3, 1), FactionPair(1, 3));
        assert_eq!(FactionPair::new(1, 3), FactionPair(1, 3));
        assert_eq!(FactionPair::new(5, 5), FactionPair(5, 5));
    }

    #[test]
    fn declare_war_cancels_coexistence_treaties() {
        let mut ledger = DiplomacyLedger::default();
        form_treaty(&mut ledger, 1, 2, TreatyKind::TradePact, 0);
        form_treaty(&mut ledger, 1, 2, TreatyKind::Alliance, 0);
        assert!(ledger.has_treaty(1, 2, TreatyKind::TradePact));
        assert!(ledger.has_treaty(1, 2, TreatyKind::Alliance));
        declare_war(&mut ledger, 1, 2, 100);
        assert!(ledger.has_treaty(1, 2, TreatyKind::War));
        assert!(!ledger.has_treaty(1, 2, TreatyKind::TradePact));
        assert!(!ledger.has_treaty(1, 2, TreatyKind::Alliance));
        // Three TreatyBroken + one TreatyFormed(War).
        let r = ledger.relation(1, 2).unwrap();
        let broken = r
            .incident_log
            .iter()
            .filter(|i| matches!(i.kind, IncidentKind::TreatyBroken(_)))
            .count();
        assert_eq!(broken, 2);
    }

    #[test]
    fn offer_peace_accepted_under_fear() {
        let mut relation = DiplomaticRelation::default();
        relation.treaties.insert(TreatyKind::War);
        relation.reputation.fear = FEAR_ACCEPT_PEACE + 5;
        assert_eq!(
            evaluate_proposal(DiplomacyProposal::OfferPeace, &relation),
            ProposalResponse::Accept
        );
    }

    #[test]
    fn alliance_rejected_when_unfamiliar() {
        let mut relation = DiplomaticRelation::default();
        relation.reputation.trust = TRUST_ACCEPT_ALLIANCE + 10;
        relation.reputation.familiarity = 10; // below gate
        assert_eq!(
            evaluate_proposal(DiplomacyProposal::OfferAlliance, &relation),
            ProposalResponse::Reject
        );
    }

    #[test]
    fn alliance_accepted_when_trusted_and_familiar() {
        let mut relation = DiplomaticRelation::default();
        relation.reputation.trust = TRUST_ACCEPT_ALLIANCE + 10;
        relation.reputation.familiarity = FAMILIARITY_ALLIANCE_GATE + 10;
        assert_eq!(
            evaluate_proposal(DiplomacyProposal::OfferAlliance, &relation),
            ProposalResponse::Accept
        );
    }

    #[test]
    fn trade_blocked_by_grievance() {
        let mut relation = DiplomaticRelation::default();
        relation.reputation.trust = 5;
        relation.reputation.grievance = GRIEVANCE_BLOCK_TRADE + 5;
        assert_eq!(
            evaluate_proposal(DiplomacyProposal::OfferTradePact, &relation),
            ProposalResponse::Reject
        );
    }

    #[test]
    fn reputation_decays_per_day_with_grievance_persisting() {
        let mut rep = Reputation {
            trust: 80,
            fear: 80,
            grievance: 80,
            familiarity: 100,
        };
        // After 30 days: trust still strongly positive (well above 0),
        // fear noticeably reduced (~10-day half-life), grievance
        // barely budged (~365-day half-life). Integer truncation
        // accelerates the analytical curve so we test relative
        // ordering + sign rather than exact half-life math.
        for _ in 0..30 {
            rep.decay_one_day();
        }
        assert!(rep.trust > 30 && rep.trust < 80, "trust 30-day: got {}", rep.trust);
        assert!(rep.fear < 20 && rep.fear >= 0, "fear 30-day: got {}", rep.fear);
        assert!(
            rep.grievance > 70,
            "grievance should persist 30 days, got {}",
            rep.grievance
        );
        // Familiarity never decays.
        assert_eq!(rep.familiarity, 100);
        // After many more days, fear drops near zero but grievance
        // remains. Round-to-nearest sticks at very small values; the
        // game-relevant invariant is "fear ≪ grievance after long idle".
        for _ in 0..200 {
            rep.decay_one_day();
        }
        assert!(rep.fear < 10, "fear should be near zero, got {}", rep.fear);
        assert!(
            rep.grievance > rep.fear * 3,
            "grievance should dominate after long idle: g={} f={}",
            rep.grievance,
            rep.fear
        );
    }

    #[test]
    fn proposal_lifecycle_alloc_post_consume() {
        let mut ledger = DiplomacyLedger::default();
        let id = ledger.post_proposal(1, 2, DiplomacyProposal::OfferTradePact, 0);
        assert!(id.0 > 0);
        assert_eq!(ledger.inbox_by_faction.get(&2).unwrap().len(), 1);
        let p = ledger.consume_proposal(id).unwrap();
        assert_eq!(p.from_faction, 1);
        assert_eq!(p.to_faction, 2);
        assert!(ledger.proposals.is_empty());
        assert!(ledger.inbox_by_faction.get(&2).unwrap().is_empty());
    }

    #[test]
    fn accept_offer_trade_pact_sets_treaty() {
        let mut ledger = DiplomacyLedger::default();
        apply_accepted_proposal(&mut ledger, 1, 2, DiplomacyProposal::OfferTradePact, 0);
        assert!(ledger.has_treaty(1, 2, TreatyKind::TradePact));
    }

    #[test]
    fn accept_offer_peace_clears_war_and_sets_non_aggression() {
        let mut ledger = DiplomacyLedger::default();
        declare_war(&mut ledger, 1, 2, 0);
        apply_accepted_proposal(&mut ledger, 1, 2, DiplomacyProposal::OfferPeace, 1);
        assert!(!ledger.has_treaty(1, 2, TreatyKind::War));
        assert!(ledger.has_treaty(1, 2, TreatyKind::NonAggression));
    }

    #[test]
    fn record_raid_incident_bumps_grievance() {
        let mut ledger = DiplomacyLedger::default();
        record_incident(&mut ledger, 1, 2, 0, IncidentKind::Raid { stolen_food: 12 });
        let r = ledger.relation(1, 2).unwrap();
        assert_eq!(r.reputation.grievance, GRIEVANCE_RAID);
        assert!(r.reputation.trust < 0);
    }

    #[test]
    fn bincode_roundtrip_proposal_variants() {
        for p in [
            DiplomacyProposal::OfferTradePact,
            DiplomacyProposal::OfferAlliance,
            DiplomacyProposal::OfferPeace,
            DiplomacyProposal::OfferNonAggression,
            DiplomacyProposal::DemandTribute,
            DiplomacyProposal::OfferAid { resource_id: 7, qty: 32 },
        ] {
            let bytes = bincode::serialize(&p).unwrap();
            let back: DiplomacyProposal = bincode::deserialize(&bytes).unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn bincode_roundtrip_treaty_kind() {
        for k in [
            TreatyKind::TradePact,
            TreatyKind::Alliance,
            TreatyKind::NonAggression,
            TreatyKind::War,
        ] {
            let bytes = bincode::serialize(&k).unwrap();
            let back: TreatyKind = bincode::deserialize(&bytes).unwrap();
            assert_eq!(k, back);
        }
    }
}
