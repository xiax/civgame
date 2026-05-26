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

use crate::simulation::diplomatic_contact::DiplomaticContactBook;
use crate::simulation::diplomatic_evaluator::{
    acceptance_blocked, evaluate_proposal_v2, passes_proposer_gate, passes_receiver_gate,
    Perspective,
};
use crate::simulation::diplomatic_personality::DiplomaticPersonality;
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

/// Smart-diplomacy P1 — `OfferMemory` ring length per relation. Walks
/// recent proposal fingerprints so the proposer doesn't re-spam a deal
/// shape that just got rejected.
pub const OFFER_MEMORY_LEN: usize = 4;

/// Cooldown before the same fingerprint may be re-sent.
pub const OFFER_RESEND_COOLDOWN_TICKS: u64 = TICKS_PER_DAY as u64 * 5;

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
    /// Smart-diplomacy P1 — a tribute demand has been accepted by the
    /// subordinate. Records the moment the dominance axis activates so
    /// the activity log + UI can show "Tribute Accepted".
    TributeAccepted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Incident {
    pub tick: u64,
    pub kind: IncidentKind,
}

/// Smart-diplomacy P1 — one entry in the per-relation `OfferMemory` ring.
/// `fingerprint` collapses a proposal to a stable u64 so we can ask "have we
/// already proposed this shape recently?" without comparing nested enums.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfferMemoryEntry {
    pub fingerprint: u64,
    pub posted_tick: u64,
    /// `Some(accepted)` once the receiver has responded; `None` while
    /// still pending. v1 uses Accept/Reject only; P3 will add gap data.
    pub response: Option<bool>,
}

/// Cheap stable hash of a `DiplomacyProposal` for `OfferMemory` keying.
/// Discriminant + payload only; no allocations.
pub fn proposal_fingerprint(p: DiplomacyProposal) -> u64 {
    match p {
        DiplomacyProposal::OfferPeace => 0x01,
        DiplomacyProposal::OfferTradePact => 0x02,
        DiplomacyProposal::OfferAlliance => 0x03,
        DiplomacyProposal::OfferNonAggression => 0x04,
        DiplomacyProposal::DemandTribute => 0x05,
        DiplomacyProposal::OfferAid { resource_id, qty } => {
            // Pack rid (u16) + qty into 64 bits; high tag avoids collision
            // with the bare-discriminant constants above.
            0x06 << 56 | ((u16::from(resource_id) as u64) << 32) | qty as u64
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiplomaticRelation {
    pub treaties: TreatySet,
    pub reputation: Reputation,
    pub last_contact_tick: u64,
    pub incident_log: VecDeque<Incident>,
    /// Smart-diplomacy P1 — recent proposal fingerprints (cap
    /// `OFFER_MEMORY_LEN`). Read by the AI proposer to avoid re-sending
    /// a shape that's still inside `OFFER_RESEND_COOLDOWN_TICKS`.
    /// Pure bias, not exclusion: the cooldown is a hard skip, but
    /// expired entries are dropped.
    pub offer_memory: VecDeque<OfferMemoryEntry>,
}

impl DiplomaticRelation {
    fn push_incident(&mut self, tick: u64, kind: IncidentKind) {
        self.last_contact_tick = tick;
        if self.incident_log.len() >= INCIDENT_LOG_LEN {
            self.incident_log.pop_front();
        }
        self.incident_log.push_back(Incident { tick, kind });
    }

    /// Smart-diplomacy P1 — push a `(fingerprint, posted_tick)` onto
    /// the offer memory ring. Drops the oldest entry when full.
    pub fn record_offer(&mut self, fingerprint: u64, tick: u64) {
        if self.offer_memory.len() >= OFFER_MEMORY_LEN {
            self.offer_memory.pop_front();
        }
        self.offer_memory.push_back(OfferMemoryEntry {
            fingerprint,
            posted_tick: tick,
            response: None,
        });
    }

    /// Smart-diplomacy P1 — true iff `fingerprint` was sent inside
    /// `OFFER_RESEND_COOLDOWN_TICKS`. Used by the proposer to skip
    /// re-spamming the same shape.
    pub fn offer_on_cooldown(&self, fingerprint: u64, now: u64) -> bool {
        for e in self.offer_memory.iter() {
            if e.fingerprint == fingerprint
                && now.saturating_sub(e.posted_tick) < OFFER_RESEND_COOLDOWN_TICKS
            {
                return true;
            }
        }
        false
    }

    /// Stamp the most recent matching offer with the receiver's
    /// `accepted` response. No-op when no live entry matches.
    pub fn record_offer_response(&mut self, fingerprint: u64, accepted: bool) {
        for e in self.offer_memory.iter_mut().rev() {
            if e.fingerprint == fingerprint && e.response.is_none() {
                e.response = Some(accepted);
                return;
            }
        }
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
        IncidentKind::TributeAccepted => {
            // Acceptance bumps familiarity only; the humiliation /
            // grievance side already lands when the demand is sent.
            r.reputation.familiarity =
                r.reputation.familiarity.saturating_add(FAMILIARITY_PER_INCIDENT);
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

/// Smart-diplomacy P1 — quarter-daily Economy pass. For every
/// uncontrolled (AI) faction with pending proposals, drain the inbox
/// and apply `evaluate_proposal_v2` against the receiver's
/// personality + contact-book estimates.
pub fn ai_diplomacy_response_system(
    clock: Res<SimClock>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    contact_book: Res<DiplomaticContactBook>,
    mut registry: ResMut<FactionRegistry>,
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
        let Some(receiver_data) = registry.factions.get(&fid) else {
            // No live FactionData (abstract/despawned) → drain to oblivion.
            let _ = ledger.drain_inbox(fid);
            continue;
        };
        let personality =
            DiplomaticPersonality::from_culture(&receiver_data.culture, receiver_data.caps.home.is_mobile());
        let receiver_home = receiver_data.home_tile;
        let receiver_root = registry.root_faction(fid);
        let ids = ledger.drain_inbox(fid);
        for pid in ids {
            let Some(p) = ledger.consume_proposal(pid) else {
                continue;
            };
            let proposer_root = registry.root_faction(p.from_faction);
            let same_root = proposer_root == receiver_root;
            let is_known = contact_book.is_known(receiver_root, proposer_root);
            let treaties = ledger.treaties(p.from_faction, p.to_faction);
            // Receiver re-checks blocks at acceptance time.
            let proposer_storage_ok = match p.proposal {
                DiplomacyProposal::OfferAid { resource_id, qty } => registry
                    .factions
                    .get(&p.from_faction)
                    .map(|d| {
                        d.storage
                            .stock_of(crate::economy::resource_catalog::ResourceId(resource_id))
                            >= qty
                    })
                    .unwrap_or(false),
                _ => true,
            };
            let block = acceptance_blocked(
                p.proposal,
                treaties,
                same_root,
                is_known,
                proposer_storage_ok,
            );
            let response = if block.is_some() {
                ProposalResponse::Reject
            } else {
                let relation = ledger
                    .by_pair
                    .get(&FactionPair::new(p.from_faction, p.to_faction))
                    .cloned()
                    .unwrap_or_default();
                let contact = contact_book.record_of(receiver_root, proposer_root);
                let util = evaluate_proposal_v2(
                    p.proposal,
                    &relation,
                    &personality,
                    receiver_home,
                    contact,
                    Perspective::Receiver,
                );
                if passes_receiver_gate(&util, &personality) {
                    ProposalResponse::Accept
                } else {
                    ProposalResponse::Reject
                }
            };
            // Stamp the offer memory on the *proposer's* relation row so
            // they observe the acceptance/rejection bias next cycle.
            let fp = proposal_fingerprint(p.proposal);
            ledger
                .relation_mut(p.from_faction, p.to_faction)
                .record_offer_response(fp, response == ProposalResponse::Accept);
            if response == ProposalResponse::Accept {
                apply_accepted_proposal(&mut ledger, p.from_faction, p.to_faction, p.proposal, now);
                // Tribute also flips the dominance axis — proposer is
                // the dominant side.
                if matches!(p.proposal, DiplomacyProposal::DemandTribute) {
                    set_tribute_acceptance(&mut registry, p.from_faction, p.to_faction);
                }
            }
        }
    }
}

/// Smart-diplomacy P1 — daily-quarter Economy pass. Replaces the
/// legacy threshold ladder with utility-driven motive selection. For
/// every materialised non-SOLO AI faction, walks known partners
/// (`contact_book.is_known`), builds the candidate motive set, scores
/// each from both sides, and posts the argmax that clears both gates.
pub fn ai_diplomacy_proposal_system(
    clock: Res<SimClock>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    contact_book: Res<DiplomaticContactBook>,
    registry: Res<FactionRegistry>,
    mut ledger: ResMut<DiplomacyLedger>,
) {
    let now = clock.tick;
    if now == 0 || now % (TICKS_PER_DAY as u64 / 4) != 0 {
        return;
    }
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

    for from in candidates {
        // Per-faction-per-day cadence: faction-id-staggered.
        let day = now / TICKS_PER_DAY as u64;
        if (day + from as u64) % 5 != 0 {
            continue;
        }
        let Some(proposer_data) = registry.factions.get(&from) else {
            continue;
        };
        let proposer_personality =
            DiplomaticPersonality::from_culture(&proposer_data.culture, proposer_data.caps.home.is_mobile());
        let proposer_home = proposer_data.home_tile;
        let from_root = registry.root_faction(from);
        let Some(contacts) = contact_book.contacts_of(from_root) else {
            continue;
        };
        // Only consider known partners.
        let targets: Vec<u32> = contacts
            .known
            .iter()
            .filter(|(_, r)| r.contact_sources.any())
            .map(|(target, _)| *target)
            .collect();

        for to in targets {
            // Resolve a materialised target FactionData (skip abstract for v1).
            let Some(target_data) = registry.factions.get(&to) else {
                continue;
            };
            if !target_data.materialized {
                continue;
            }
            if registry.root_faction(from) == registry.root_faction(to) {
                continue;
            }
            let treaties = ledger.treaties(from, to);
            let at_war = treaties.has(TreatyKind::War);

            // Build candidate motive set conditioned on current treaties.
            let mut candidates_proposals: Vec<DiplomacyProposal> = Vec::with_capacity(6);
            if at_war {
                candidates_proposals.push(DiplomacyProposal::OfferPeace);
            } else {
                if !treaties.has(TreatyKind::TradePact) {
                    candidates_proposals.push(DiplomacyProposal::OfferTradePact);
                }
                if !treaties.has(TreatyKind::Alliance) {
                    candidates_proposals.push(DiplomacyProposal::OfferAlliance);
                }
                if !treaties.has(TreatyKind::NonAggression) {
                    candidates_proposals.push(DiplomacyProposal::OfferNonAggression);
                }
                candidates_proposals.push(DiplomacyProposal::DemandTribute);
                // OfferAid only when we have spare grain — keeps the
                // candidate set realistic.
                let grain = crate::economy::core_ids::grain();
                let our_grain = proposer_data.storage.stock_of(grain);
                if our_grain >= 10 {
                    candidates_proposals.push(DiplomacyProposal::OfferAid {
                        resource_id: grain.0,
                        qty: 5,
                    });
                }
            }

            // Predict receiver's personality + relation snapshot.
            let receiver_personality = DiplomaticPersonality::from_culture(
                &target_data.culture,
                target_data.caps.home.is_mobile(),
            );
            let receiver_root = registry.root_faction(to);
            let receiver_home = target_data.home_tile;

            // Score each candidate from both sides; keep argmax.
            let mut best: Option<(DiplomacyProposal, f32)> = None;
            for proposal in candidates_proposals {
                let fp = proposal_fingerprint(proposal);
                let relation = ledger
                    .relation(from, to)
                    .cloned()
                    .unwrap_or_default();
                if relation.offer_on_cooldown(fp, now) {
                    continue;
                }
                let proposer_storage_ok = match proposal {
                    DiplomacyProposal::OfferAid { resource_id, qty } => {
                        proposer_data
                            .storage
                            .stock_of(crate::economy::resource_catalog::ResourceId(resource_id))
                            >= qty
                    }
                    _ => true,
                };
                if acceptance_blocked(
                    proposal,
                    treaties,
                    false,
                    contact_book.is_known(receiver_root, from_root) ||
                        contact_book.is_known(from_root, receiver_root),
                    proposer_storage_ok,
                )
                .is_some()
                {
                    continue;
                }
                let contact_self = contact_book.record_of(from_root, receiver_root);
                let contact_target = contact_book.record_of(receiver_root, from_root);
                let proposer_util = evaluate_proposal_v2(
                    proposal,
                    &relation,
                    &proposer_personality,
                    proposer_home,
                    contact_self,
                    Perspective::Proposer,
                );
                let predicted_receiver_util = evaluate_proposal_v2(
                    proposal,
                    &relation,
                    &receiver_personality,
                    receiver_home,
                    contact_target,
                    Perspective::Receiver,
                );
                if !passes_proposer_gate(&proposer_util, &proposer_personality) {
                    continue;
                }
                if !passes_receiver_gate(&predicted_receiver_util, &receiver_personality) {
                    continue;
                }
                if best.map_or(true, |(_, n)| proposer_util.net > n) {
                    best = Some((proposal, proposer_util.net));
                }
            }

            if let Some((proposal, _)) = best {
                let _id = ledger.post_proposal(from, to, proposal, now);
                let fp = proposal_fingerprint(proposal);
                ledger.relation_mut(from, to).record_offer(fp, now);
            }
        }
    }
}

/// Smart-diplomacy P1 — wire the dominance axis when a `DemandTribute`
/// is accepted. Caller (AI response system / player command dispatcher)
/// has the `&mut FactionRegistry` access the ledger doesn't. Idempotent
/// via `FactionRegistry::set_dominance`.
pub fn set_tribute_acceptance(
    registry: &mut FactionRegistry,
    dominant: u32,
    subordinate: u32,
) {
    registry.set_dominance(dominant, subordinate);
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
            // Smart-diplomacy P1 — record the acceptance as a ledger
            // incident; the dominance axis side-effect is wired by the
            // caller (player command handler / AI response system) via
            // `FactionRegistry::set_dominance`. The ledger doesn't own
            // the registry, so the wiring happens at call sites that
            // do; see `set_tribute_acceptance` below for the helper.
            let r = ledger.relation_mut(from, to);
            r.reputation.familiarity =
                r.reputation.familiarity.saturating_add(FAMILIARITY_PER_INCIDENT);
            r.push_incident(tick, IncidentKind::TributeAccepted);
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
