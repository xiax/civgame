//! Federation overlay on the pairwise diplomacy ledger.
//!
//! A `Federation` is a name + member roster. Its only durable mechanical
//! effect is that `federation_alliance_sync_system` (Economy, daily,
//! before `treaty_to_grant_sync_system`) maintains pairwise `Alliance`
//! between every co-member pair. Pairwise Alliance already drives:
//!
//! - `access_grant::reconcile_pair_grants` → symmetric `FullTerritory`
//! - `raid::pick_raid_target` → co-members unraidable
//! - `trespass::classify_trespass` → co-members pass through territory
//!
//! Federation membership keys on **root** faction ids (households
//! resolved via `FactionRegistry::root_faction` at every command boundary).
//!
//! Two pieces don't fall out of pairwise Alliance and live here:
//!
//! 1. **Defensive propagation** — `propagate_federation_defense_on_war`.
//!    When an outsider attacks one co-member, every other co-member
//!    declares war on the attacker. Defensive-only: a co-member's
//!    offensive war does NOT bandwagon the bloc.
//! 2. **Intra-fed auto-expulsion** — `expel_on_intra_fed_hostility`.
//!    Co-member attacks co-member → attacker drops from the federation;
//!    the sync system tears their Alliance treaties down next tick.
//!
//! Trade-trust spread (`record_trade_incident_with_propagation`) bumps
//! `IncidentKind::TradeCompleted` on bystander co-member pairs at
//! `FederationCharter::trade_propagation_pct` of the original value
//! (cap 5 members ⇒ ≤8 pair-writes per trade).

use ahash::AHashMap;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::simulation::diplomacy::{
    break_treaty, declare_war, form_treaty, record_incident, DiplomacyLedger, IncidentKind,
    TreatyKind,
};
use crate::simulation::faction::FactionRegistry;
use crate::simulation::schedule::SimClock;
use crate::world::seasons::TICKS_PER_DAY;

// ── Constants ────────────────────────────────────────────────────────────

/// Hard cap on members per federation (user-decision). Bounds
/// trade-propagation fan-out and keeps the UI roster compact.
pub const FEDERATION_MEMBER_CAP: usize = 5;

/// Default trust-spread for `record_trade_incident_with_propagation`.
pub const DEFAULT_TRADE_PROPAGATION_PCT: f32 = 0.20;

/// Pending-invite expiry — same as `PROPOSAL_EXPIRY_TICKS`.
pub const FEDERATION_INVITE_EXPIRY_TICKS: u64 = (TICKS_PER_DAY as u64) * 7;

/// Reputation penalty applied to every former co-member pair on
/// voluntary leave or expulsion.
pub const FEDERATION_LEAVE_TRUST_PENALTY: i16 = 10;
pub const FEDERATION_LEAVE_GRIEVANCE_PENALTY: i16 = 4;

// ── Data model ───────────────────────────────────────────────────────────

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct FederationId(pub u32);

impl FederationId {
    pub const NONE: FederationId = FederationId(0);
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FederationCharter {
    /// Trade-trust spread to bystander co-members. `0.20` = 20% of the
    /// originating pair's trust bump lands on every other (member,
    /// originating-party) pair.
    pub trade_propagation_pct: f32,
    /// In v1, always `true`. First intra-fed hostile incident drops the
    /// aggressor from the federation.
    pub auto_expel_on_intra_attack: bool,
}

impl Default for FederationCharter {
    fn default() -> Self {
        Self {
            trade_propagation_pct: DEFAULT_TRADE_PROPAGATION_PCT,
            auto_expel_on_intra_attack: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Federation {
    pub id: FederationId,
    pub name: String,
    /// Root faction ids; kept sorted for canonical iteration.
    pub members: Vec<u32>,
    /// Root faction that issued the founding invite. Holds expulsion
    /// authority. Survives founder exit — falls to the lowest remaining
    /// member id (see `Federation::reassign_founder`).
    pub founder: u32,
    pub founded_tick: u64,
    pub charter: FederationCharter,
}

impl Federation {
    pub fn contains(&self, root: u32) -> bool {
        self.members.binary_search(&root).is_ok()
    }

    pub fn insert(&mut self, root: u32) -> bool {
        match self.members.binary_search(&root) {
            Ok(_) => false,
            Err(idx) => {
                self.members.insert(idx, root);
                true
            }
        }
    }

    fn remove(&mut self, root: u32) -> bool {
        match self.members.binary_search(&root) {
            Ok(idx) => {
                self.members.remove(idx);
                true
            }
            Err(_) => false,
        }
    }

    fn reassign_founder(&mut self) {
        if !self.members.contains(&self.founder) {
            if let Some(&first) = self.members.first() {
                self.founder = first;
            }
        }
    }
}

/// Pending invite — recorded on `ProposeFederation`, consumed by
/// `AcceptFederationInvite` or expired by `federation_invite_expiry_system`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingFederationInvite {
    pub federation_id: FederationId,
    pub from_faction: u32,
    pub to_faction: u32,
    pub name: String,
    pub posted_tick: u64,
}

#[derive(Resource, Default)]
pub struct FederationMap {
    pub by_id: AHashMap<FederationId, Federation>,
    pub by_root_faction: AHashMap<u32, FederationId>,
    pub invites: AHashMap<(FederationId, u32), PendingFederationInvite>,
    pub next_id: u32,
}

impl FederationMap {
    pub fn alloc_id(&mut self) -> FederationId {
        self.next_id += 1;
        FederationId(self.next_id)
    }

    /// Federation that `root` belongs to (if any).
    pub fn federation_of(&self, root: u32) -> Option<&Federation> {
        self.by_root_faction
            .get(&root)
            .and_then(|id| self.by_id.get(id))
    }

    /// Whether two **root** faction ids share a federation.
    pub fn co_members(&self, a: u32, b: u32) -> bool {
        if a == b {
            return false;
        }
        match self.by_root_faction.get(&a) {
            Some(fid) => self.by_root_faction.get(&b) == Some(fid),
            None => false,
        }
    }

    /// All co-members of `root` excluding `root` itself.
    pub fn co_members_of(&self, root: u32) -> Vec<u32> {
        let Some(fid) = self.by_root_faction.get(&root) else {
            return Vec::new();
        };
        let Some(fed) = self.by_id.get(fid) else {
            return Vec::new();
        };
        fed.members.iter().copied().filter(|&m| m != root).collect()
    }

    /// Drop a member from the federation. Disbands the federation if
    /// fewer than two members remain — a one-member "federation" is
    /// pointless and would survive into UI / sync forever otherwise.
    pub fn remove_member(&mut self, root: u32) -> Option<FederationId> {
        let fid = self.by_root_faction.remove(&root)?;
        let disband = {
            let Some(fed) = self.by_id.get_mut(&fid) else {
                return Some(fid);
            };
            fed.remove(root);
            fed.reassign_founder();
            fed.members.len() < 2
        };
        if disband {
            if let Some(fed) = self.by_id.remove(&fid) {
                for m in fed.members {
                    self.by_root_faction.remove(&m);
                }
            }
        }
        Some(fid)
    }
}

/// Reason an `ActivityEntryKind::FederationExpelled` was emitted.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum ExpulsionReason {
    Voluntary,
    IntraFedAttack,
}

// ── Sync system ──────────────────────────────────────────────────────────

/// Walk every federation's co-member pairs and ensure pairwise Alliance.
/// Tear down Alliance for pairs that **were** co-members and no longer are.
///
/// Idempotent on both sides — `form_treaty` returns silently when the
/// bit is set, `break_treaty` no-ops when unset. Runs daily before
/// `treaty_to_grant_sync_system` so access grants land same tick.
///
/// "Was co-member" is detected via `RememberedFederationAlliance` (per-pair
/// memory of "we were once federated, so this Alliance is federation-derived").
/// Without this, tearing down membership couldn't distinguish a
/// player-formed Alliance from a sync-installed one.
pub fn federation_alliance_sync_system(
    clock: Res<SimClock>,
    map: Res<FederationMap>,
    mut ledger: ResMut<DiplomacyLedger>,
    mut memory: ResMut<RememberedFederationAlliance>,
) {
    if clock.tick == 0 || clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick;
    let mut live_pairs: ahash::AHashSet<(u32, u32)> = ahash::AHashSet::new();
    for fed in map.by_id.values() {
        for i in 0..fed.members.len() {
            for j in (i + 1)..fed.members.len() {
                let a = fed.members[i];
                let b = fed.members[j];
                let pair = if a <= b { (a, b) } else { (b, a) };
                live_pairs.insert(pair);
                // Skip when at war — declaring war already broke the
                // Alliance, and `form_treaty` would reject anyway. The
                // pair stays in `live_pairs` so the post-loop teardown
                // doesn't fire on it.
                if ledger
                    .relation(a, b)
                    .map(|r| r.treaties.has(TreatyKind::War))
                    .unwrap_or(false)
                {
                    continue;
                }
                let _ = form_treaty(&mut ledger, a, b, TreatyKind::Alliance, now);
                memory.pairs.insert(pair);
            }
        }
    }
    let stale: Vec<(u32, u32)> = memory
        .pairs
        .iter()
        .copied()
        .filter(|p| !live_pairs.contains(p))
        .collect();
    for pair in stale {
        if ledger
            .relation(pair.0, pair.1)
            .map(|r| r.treaties.has(TreatyKind::Alliance))
            .unwrap_or(false)
        {
            break_treaty(&mut ledger, pair.0, pair.1, TreatyKind::Alliance, now);
        }
        memory.pairs.remove(&pair);
    }
}

/// Per-pair flag — was the live Alliance installed by the federation
/// sync? Used to tell sync-derived Alliances from player-formed ones at
/// teardown time. Resource because it lives across sync ticks.
#[derive(Resource, Default)]
pub struct RememberedFederationAlliance {
    pub pairs: ahash::AHashSet<(u32, u32)>,
}

/// Daily GC for expired invites.
pub fn federation_invite_expiry_system(
    clock: Res<SimClock>,
    mut map: ResMut<FederationMap>,
) {
    if clock.tick == 0 || clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick;
    let expired: Vec<(FederationId, u32)> = map
        .invites
        .iter()
        .filter(|(_, inv)| now.saturating_sub(inv.posted_tick) >= FEDERATION_INVITE_EXPIRY_TICKS)
        .map(|(k, _)| *k)
        .collect();
    for key in expired {
        map.invites.remove(&key);
    }
}

// ── Defensive propagation ────────────────────────────────────────────────

/// Outsider attacks `victim_root` → every other co-member of the victim
/// declares war on `attacker_root`. Idempotent (declare_war is).
///
/// Defensive-only: a co-member's offensive war does NOT cascade. The
/// caller decides whether to propagate (raid + Hostile trespass do;
/// player-initiated declarations do not).
pub fn propagate_federation_defense_on_war(
    map: &FederationMap,
    ledger: &mut DiplomacyLedger,
    attacker_root: u32,
    victim_root: u32,
    tick: u64,
) -> Vec<u32> {
    let mut bandwagoners = Vec::new();
    if attacker_root == victim_root {
        return bandwagoners;
    }
    // Don't cascade if attacker is also in the victim's federation —
    // that's an intra-fed attack, handled by `expel_on_intra_fed_hostility`.
    if map.co_members(attacker_root, victim_root) {
        return bandwagoners;
    }
    let co = map.co_members_of(victim_root);
    for m in co {
        if m == attacker_root {
            continue;
        }
        declare_war(ledger, attacker_root, m, tick);
        bandwagoners.push(m);
    }
    bandwagoners
}

/// If `attacker_root` and `victim_root` are co-members of the same
/// federation, drop the attacker and return the federation id. The
/// sync system tears down their Alliance treaties next tick. Penalises
/// every former co-member pair: trust drops, grievance rises.
///
/// Caller emits one `ActivityEntryKind::FederationExpelled` per affected
/// co-member (we don't write activity-log events from sim core to keep
/// the dependency one-way).
pub fn expel_on_intra_fed_hostility(
    map: &mut FederationMap,
    ledger: &mut DiplomacyLedger,
    attacker_root: u32,
    victim_root: u32,
    tick: u64,
) -> Option<(FederationId, Vec<u32>)> {
    if !map.co_members(attacker_root, victim_root) {
        return None;
    }
    let Some(fed_id) = map.by_root_faction.get(&attacker_root).copied() else {
        return None;
    };
    if !map
        .by_id
        .get(&fed_id)
        .map(|f| f.charter.auto_expel_on_intra_attack)
        .unwrap_or(true)
    {
        return None;
    }
    let former = map.co_members_of(attacker_root);
    map.remove_member(attacker_root);
    for other in &former {
        if let Some(rel) = map
            .by_id
            .get(&fed_id) // may already be despawned if disbanded
            .map(|_| ())
        {
            let _ = rel;
        }
        let r = ledger.relation_mut(attacker_root, *other);
        r.reputation.trust = r.reputation.trust.saturating_sub(FEDERATION_LEAVE_TRUST_PENALTY);
        r.reputation.grievance = r
            .reputation
            .grievance
            .saturating_add(FEDERATION_LEAVE_GRIEVANCE_PENALTY);
        r.reputation.clamp();
    }
    let _ = tick; // unused now; reserved for future incident logging
    Some((fed_id, former))
}

// ── Trade-trust spread ───────────────────────────────────────────────────

/// `IncidentKind::TradeCompleted` between two co-members propagates to
/// each bystander co-member as a smaller `TradeCompleted` against the
/// **originating-party** (deterministic: the lexicographically smaller
/// root). Caller passes the original currency value; we scale by
/// `FederationCharter::trade_propagation_pct`.
///
/// With member cap 5: ≤4 propagation pairs × 2 records (one per
/// originating party) = ≤8 pair-writes per trade — trivial.
pub fn record_trade_incident_with_propagation(
    map: &FederationMap,
    ledger: &mut DiplomacyLedger,
    a: u32,
    b: u32,
    value_currency: u32,
    tick: u64,
) {
    record_incident(
        ledger,
        a,
        b,
        tick,
        IncidentKind::TradeCompleted { value_currency },
    );
    if !map.co_members(a, b) {
        return;
    }
    let Some(fed_id) = map.by_root_faction.get(&a).copied() else {
        return;
    };
    let Some(fed) = map.by_id.get(&fed_id) else {
        return;
    };
    let pct = fed.charter.trade_propagation_pct.clamp(0.0, 1.0);
    if pct <= 0.0 {
        return;
    }
    let spread_value = ((value_currency as f32) * pct).round() as u32;
    if spread_value == 0 {
        return;
    }
    let originating = if a <= b { a } else { b };
    for &m in &fed.members {
        if m == a || m == b {
            continue;
        }
        record_incident(
            ledger,
            originating,
            m,
            tick,
            IncidentKind::TradeCompleted {
                value_currency: spread_value,
            },
        );
    }
}

// ── Validation helpers (pure-fn, for tests + command handlers) ───────────

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FederationJoinError {
    SelfTarget,
    AlreadyInFederation,
    FederationFull,
    UnknownFederation,
    NotInvited,
}

/// Can `root_faction` accept invite `(fed_id, root_faction)`?
pub fn validate_accept_invite(
    map: &FederationMap,
    fed_id: FederationId,
    root_faction: u32,
) -> Result<(), FederationJoinError> {
    if map.by_root_faction.contains_key(&root_faction) {
        return Err(FederationJoinError::AlreadyInFederation);
    }
    let Some(fed) = map.by_id.get(&fed_id) else {
        // Founding-accept (federation entity materialises on first
        // accept) — caller validates the founder side separately.
        if map
            .invites
            .keys()
            .any(|(fid, to)| *fid == fed_id && *to == root_faction)
        {
            return Ok(());
        }
        return Err(FederationJoinError::UnknownFederation);
    };
    if fed.members.len() >= FEDERATION_MEMBER_CAP {
        return Err(FederationJoinError::FederationFull);
    }
    if !map
        .invites
        .contains_key(&(fed_id, root_faction))
    {
        return Err(FederationJoinError::NotInvited);
    }
    Ok(())
}

// ── AI proposer ──────────────────────────────────────────────────────────

/// Daily AI pass — for each uncontrolled (non-player) faction not in a
/// federation, check whether any two known contacts both hold Alliance
/// with us AND share a current war target. If so, propose a federation
/// invite to both (and the auto-accept loop closes it next pass).
///
/// Keeps cadence staggered (`faction_id % 7`) and skips factions inside
/// `FederationCharter`-default federations.
pub fn ai_federation_proposal_system(
    clock: Res<SimClock>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    contact_book: Res<crate::simulation::diplomatic_contact::DiplomaticContactBook>,
    registry: Res<FactionRegistry>,
    ledger: Res<DiplomacyLedger>,
    mut map: ResMut<FederationMap>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
) {
    let now = clock.tick;
    if now == 0 || now % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let candidates: Vec<u32> = registry
        .factions
        .iter()
        .filter(|(fid, data)| {
            **fid != crate::simulation::faction::SOLO
                && !controlled.contains(**fid)
                && data.parent_faction.is_none()
                && data.materialized
        })
        .map(|(fid, _)| *fid)
        .collect();
    let day = now / TICKS_PER_DAY as u64;
    for from in candidates {
        if (day + from as u64) % 7 != 0 {
            continue;
        }
        let from_root = registry.root_faction(from);
        if map.by_root_faction.contains_key(&from_root) {
            continue;
        }
        let Some(contacts) = contact_book.contacts_of(from_root) else {
            continue;
        };
        // Allies: known contacts holding Alliance with us, not already
        // in any federation.
        let allies: Vec<u32> = contacts
            .known
            .iter()
            .filter_map(|(target, _)| {
                let troot = registry.root_faction(*target);
                if troot == from_root
                    || map.by_root_faction.contains_key(&troot)
                    || !ledger.has_treaty(from_root, troot, TreatyKind::Alliance)
                {
                    None
                } else {
                    Some(troot)
                }
            })
            .collect();
        if allies.len() < 2 {
            continue;
        }
        // Score: need a shared war target. Pick first ally-pair that
        // both have ≥1 common live `War` target.
        let mut pick: Option<(u32, u32)> = None;
        'outer: for i in 0..allies.len() {
            for j in (i + 1)..allies.len() {
                let a = allies[i];
                let b = allies[j];
                let shared = ledger
                    .by_pair
                    .iter()
                    .any(|(pair, rel)| {
                        rel.treaties.has(TreatyKind::War)
                            && pair.contains(a)
                            && ledger.has_treaty(b, pair.other(a).unwrap_or(b), TreatyKind::War)
                    });
                if shared {
                    pick = Some((a, b));
                    break 'outer;
                }
            }
        }
        let Some((a, b)) = pick else { continue };
        // Create federation; seat founder; invite both allies.
        let fed_id = map.alloc_id();
        let name = format!("Bloc of {}", from_root);
        map.by_id.insert(
            fed_id,
            Federation {
                id: fed_id,
                name: name.clone(),
                members: vec![from_root],
                founder: from_root,
                founded_tick: now,
                charter: FederationCharter::default(),
            },
        );
        map.by_root_faction.insert(from_root, fed_id);
        for inv in [a, b] {
            map.invites.insert(
                (fed_id, inv),
                PendingFederationInvite {
                    federation_id: fed_id,
                    from_faction: from_root,
                    to_faction: inv,
                    name: name.clone(),
                    posted_tick: now,
                },
            );
        }
        // Surface to the player's activity log only if either party is
        // controlled by them; otherwise the AI bloc forms silently.
        if controlled.contains(a) || controlled.contains(b) || controlled.contains(from_root) {
            activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                tick: now,
                actor: bevy::prelude::Entity::PLACEHOLDER,
                faction_id: from_root,
                kind: crate::ui::activity_log::ActivityEntryKind::FederationFormed {
                    federation_id: fed_id,
                    name,
                    members: vec![from_root],
                },
            });
        }
    }
}

/// Daily AI pass — drain federation invites addressed to uncontrolled
/// factions. Accept when the inviting federation already holds Alliance
/// with the receiver (acceptance signal is "do we already trust this
/// bloc enough to join").
pub fn ai_federation_response_system(
    clock: Res<SimClock>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    registry: Res<FactionRegistry>,
    ledger: Res<DiplomacyLedger>,
    mut map: ResMut<FederationMap>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
) {
    let now = clock.tick;
    if now == 0 || now % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let candidates: Vec<(FederationId, u32, u32)> = map
        .invites
        .iter()
        .filter(|((_, to), _)| {
            !controlled.contains(*to)
                && registry
                    .factions
                    .get(to)
                    .map(|d| d.parent_faction.is_none() && d.materialized)
                    .unwrap_or(false)
        })
        .map(|((fid, to), inv)| (*fid, *to, inv.from_faction))
        .collect();
    for (fid, to, _from) in candidates {
        // Accept gate: caller already in a federation? skip.
        if map.by_root_faction.contains_key(&to) {
            map.invites.remove(&(fid, to));
            continue;
        }
        let Some(fed) = map.by_id.get(&fid) else {
            map.invites.remove(&(fid, to));
            continue;
        };
        if fed.members.len() >= FEDERATION_MEMBER_CAP {
            map.invites.remove(&(fid, to));
            continue;
        }
        // Accept iff at least one existing member holds Alliance with `to`.
        let trust = fed.members.iter().any(|&m| {
            ledger.has_treaty(m, to, TreatyKind::Alliance)
                && !ledger.has_treaty(m, to, TreatyKind::War)
        });
        if !trust {
            continue;
        }
        // Drop invite, insert membership.
        map.invites.remove(&(fid, to));
        let members_after = {
            let fed = map.by_id.get_mut(&fid).unwrap();
            fed.insert(to);
            fed.members.clone()
        };
        map.by_root_faction.insert(to, fid);
        if members_after.len() == 2 {
            let name = map
                .by_id
                .get(&fid)
                .map(|f| f.name.clone())
                .unwrap_or_default();
            if controlled.ids.iter().any(|id| members_after.contains(id)) {
                activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                    tick: now,
                    actor: bevy::prelude::Entity::PLACEHOLDER,
                    faction_id: to,
                    kind: crate::ui::activity_log::ActivityEntryKind::FederationFormed {
                        federation_id: fid,
                        name,
                        members: members_after,
                    },
                });
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fed_with(members: &[u32]) -> Federation {
        let mut f = Federation {
            id: FederationId(1),
            name: "Test".into(),
            members: members.to_vec(),
            founder: members[0],
            founded_tick: 0,
            charter: FederationCharter::default(),
        };
        f.members.sort();
        f
    }

    fn map_with(members: &[u32]) -> FederationMap {
        let mut m = FederationMap::default();
        let fed = fed_with(members);
        for &mb in members {
            m.by_root_faction.insert(mb, fed.id);
        }
        m.by_id.insert(fed.id, fed);
        m.next_id = 1;
        m
    }

    #[test]
    fn co_members_detects_pair() {
        let m = map_with(&[10, 20, 30]);
        assert!(m.co_members(10, 20));
        assert!(m.co_members(20, 30));
        assert!(!m.co_members(10, 10));
        assert!(!m.co_members(10, 99));
    }

    #[test]
    fn outsider_attack_propagates_to_co_members() {
        let mut ledger = DiplomacyLedger::default();
        let m = map_with(&[10, 20, 30]);
        let bandwagoners =
            propagate_federation_defense_on_war(&m, &mut ledger, /*attacker=*/ 99, /*victim=*/ 10, 0);
        assert!(bandwagoners.contains(&20));
        assert!(bandwagoners.contains(&30));
        assert!(ledger.treaties(20, 99).has(TreatyKind::War));
        assert!(ledger.treaties(30, 99).has(TreatyKind::War));
    }

    #[test]
    fn intra_fed_hostility_does_not_propagate() {
        let mut ledger = DiplomacyLedger::default();
        let m = map_with(&[10, 20, 30]);
        let bandwagoners =
            propagate_federation_defense_on_war(&m, &mut ledger, 10, 20, 0);
        assert!(bandwagoners.is_empty());
    }

    #[test]
    fn intra_fed_hostility_expels_aggressor() {
        let mut ledger = DiplomacyLedger::default();
        let mut m = map_with(&[10, 20, 30]);
        let result = expel_on_intra_fed_hostility(&mut m, &mut ledger, 10, 20, 0);
        let (_fid, former) = result.expect("expulsion fires");
        assert!(former.contains(&20));
        assert!(former.contains(&30));
        assert!(!m.by_root_faction.contains_key(&10));
        assert!(m.co_members(20, 30), "victim + bystander still co-members");
    }

    #[test]
    fn solo_member_disbands_on_removal() {
        let mut ledger = DiplomacyLedger::default();
        let mut m = map_with(&[10, 20]);
        expel_on_intra_fed_hostility(&mut m, &mut ledger, 10, 20, 0);
        assert!(m.by_id.is_empty(), "two-member fed disbands on removal");
        assert!(!m.by_root_faction.contains_key(&20));
    }

    #[test]
    fn trade_propagates_to_bystander() {
        let mut ledger = DiplomacyLedger::default();
        let m = map_with(&[10, 20, 30]);
        record_trade_incident_with_propagation(&m, &mut ledger, 10, 20, 100, 0);
        // Originating party is min(10,20)=10. Bystander 30 gets a
        // TradeCompleted with 30's pair against 10.
        let bystander_trust = ledger
            .relation(10, 30)
            .expect("propagated incident")
            .reputation
            .trust;
        assert!(bystander_trust > 0);
    }

    #[test]
    fn validate_accept_invite_rejects_full() {
        let mut m = map_with(&[10, 20, 30, 40, 50]);
        m.invites.insert(
            (FederationId(1), 60),
            PendingFederationInvite {
                federation_id: FederationId(1),
                from_faction: 10,
                to_faction: 60,
                name: "Test".into(),
                posted_tick: 0,
            },
        );
        assert_eq!(
            validate_accept_invite(&m, FederationId(1), 60),
            Err(FederationJoinError::FederationFull)
        );
    }

    #[test]
    fn alliance_sync_forms_pairwise_alliances() {
        use crate::simulation::schedule::SimClock;
        let mut app = bevy::prelude::App::new();
        app.insert_resource(SimClock {
            tick: TICKS_PER_DAY as u64,
            ..Default::default()
        });
        app.insert_resource(DiplomacyLedger::default());
        app.insert_resource(RememberedFederationAlliance::default());
        let mut map = FederationMap::default();
        let fed = fed_with(&[10, 20, 30]);
        for &m in &fed.members {
            map.by_root_faction.insert(m, fed.id);
        }
        map.by_id.insert(fed.id, fed);
        app.insert_resource(map);
        app.add_systems(bevy::prelude::Update, federation_alliance_sync_system);
        app.update();
        let ledger = app.world().resource::<DiplomacyLedger>();
        assert!(ledger.treaties(10, 20).has(TreatyKind::Alliance));
        assert!(ledger.treaties(10, 30).has(TreatyKind::Alliance));
        assert!(ledger.treaties(20, 30).has(TreatyKind::Alliance));
    }

    #[test]
    fn alliance_sync_breaks_on_leave() {
        use crate::simulation::schedule::SimClock;
        let mut app = bevy::prelude::App::new();
        let day = TICKS_PER_DAY as u64;
        app.insert_resource(SimClock {
            tick: day,
            ..Default::default()
        });
        app.insert_resource(DiplomacyLedger::default());
        app.insert_resource(RememberedFederationAlliance::default());
        let mut map = FederationMap::default();
        let fed = fed_with(&[10, 20, 30]);
        for &m in &fed.members {
            map.by_root_faction.insert(m, fed.id);
        }
        map.by_id.insert(fed.id, fed);
        app.insert_resource(map);
        app.add_systems(bevy::prelude::Update, federation_alliance_sync_system);
        // Day 1: forms all alliances.
        app.update();
        // Drop member 30 and advance to day 2.
        {
            let mut map = app.world_mut().resource_mut::<FederationMap>();
            map.remove_member(30);
            let mut clock = app.world_mut().resource_mut::<SimClock>();
            clock.tick = day * 2;
        }
        app.update();
        let ledger = app.world().resource::<DiplomacyLedger>();
        assert!(ledger.treaties(10, 20).has(TreatyKind::Alliance));
        assert!(
            !ledger.treaties(10, 30).has(TreatyKind::Alliance),
            "removed member's alliance torn down by sync"
        );
        assert!(!ledger.treaties(20, 30).has(TreatyKind::Alliance));
    }

    #[test]
    fn validate_accept_invite_rejects_double_membership() {
        let mut m = map_with(&[10, 20]);
        m.invites.insert(
            (FederationId(2), 10),
            PendingFederationInvite {
                federation_id: FederationId(2),
                from_faction: 30,
                to_faction: 10,
                name: "Other".into(),
                posted_tick: 0,
            },
        );
        assert_eq!(
            validate_accept_invite(&m, FederationId(2), 10),
            Err(FederationJoinError::AlreadyInFederation)
        );
    }
}
