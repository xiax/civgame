# Smart Diplomacy AI Plan

## Summary
- Extend the existing diplomacy ledger into a utility-driven AI that perceives contacts, forms intentions, constructs concrete deal packages, predicts the other side’s acceptance, and follows through with courier delivery.
- v1 supports AI-to-AI and AI-to-player diplomacy for treaties, peace, tribute, aid, resource/currency exchange, market access, safe passage, and seasonal camp access.
- Factions only negotiate with factions they are aware of through scouting, visited settlements, trader contact, gossip, trespass, treaty history, or proposals.

## Core AI Model
- Add a daily staggered `ai_diplomacy_strategy_system` that runs per root faction and evaluates known contacts from `DiplomaticContactBook`.
- Each AI creates a `DiplomaticContext` snapshot:
  - Self: food/material surplus, shortages, treasury, population, military capability, culture traits, lifestyle, active raids/migration, known market node.
  - Target: reputation, treaties, access grants, distance, known wealth/stock estimates, recent incidents, last contact freshness.
  - World: war state, territory overlap, trespass pressure, shared enemies, trade route viability, courier reachability.
- Add `DiplomaticPersonality` derived from `FactionCulture`:
  - `martial` raises tribute/war pressure and lowers fear of grievance.
  - `mercantile` prefers market access, trade pacts, and resource exchange.
  - `defensive` values non-aggression and safe borders, dislikes foreign access.
  - `ceremonial` values alliances, aid, and long trust memory.
  - `density/style/lifestyle` affects territorial tolerance and seasonal camp access.
- AI does not pick random proposals directly; it scores motives, builds candidate deals, evaluates both sides, then posts the best acceptable offer.

## AI Decision Loop
- Step 1: choose diplomatic motive per known contact:
  - `EndWar`: at war, high fear, low progress, or costly raids.
  - `SecureBorder`: repeated trespass, nearby claims, defensive culture.
  - `OpenTrade`: mercantile culture, market surplus/shortage mismatch, known route.
  - `RequestAid`: famine/material shortage with positive trust or high fear of target.
  - `OfferAid`: surplus plus trust/ally interest.
  - `DemandTribute`: high martial strength advantage, target fear, low trust.
  - `RequestAccess`: route crosses target territory or nomads want seasonal camp.
  - `Punish`: ignored warnings, raids, attacks, broken obligations.
- Step 2: generate 3-8 `DealPackage` candidates from the top motives.
- Step 3: run `evaluate_deal_for(faction, package, context)` for proposer and predicted receiver.
- Step 4: apply personality thresholds:
  - Proposer must get `net_utility >= min_gain`.
  - Predicted receiver must get `net_utility >= acceptance_threshold`.
  - Risky/uncertain offers require higher expected gain.
- Step 5: post at most one proposal per pair per cooldown window; store `DiplomaticOfferMemory` so the AI avoids spamming rejected deal shapes.

## Deal Valuation
- Add a pure `DealEvaluator` that decomposes a package into signed utility:
  - `economic_value`: resource/currency terms using local market price, fallback `trade_base_value`, scarcity multiplier, surplus discount, and delivery cost.
  - `security_value`: non-aggression, alliance, peace, access risk, territory pressure, military imbalance.
  - `relationship_value`: trust, grievance, familiarity, treaty reliability, recent aid/trade/default history.
  - `strategic_value`: shared enemies, route unlocks, famine survival, migration/camp need, market access.
  - `execution_risk`: courier distance, stock uncertainty, active war/raid, obligation size, target reliability.
- Fairness is explicit:
  - `fairness_ratio = offered_value_to_receiver / requested_value_from_receiver`.
  - AI labels deals internally as `Generous`, `Fair`, `HardBargain`, `Exploitative`.
  - Friendly/ceremonial factions prefer `Fair` or better; martial factions may send `HardBargain`; `Exploitative` is only sent under tribute/fear contexts.
- Acceptance:
  - Accept if receiver utility clears threshold and no hard block applies.
  - Hard blocks: war except peace, self/same-root, unknown faction, impossible delivery, expired proposal, access requested by active raiders/drafted military, treaty conflict.
  - Grievance raises threshold; trust lowers it; fear lowers it for peace/tribute but raises suspicion for access.

## Bargaining Behavior
- Rejections are not no-ops for AI memory.
- Add `OfferMemory { proposal_fingerprint, response, tick, perceived_gap }`.
- After rejection:
  - If close to fair, AI may retry later with a concession.
  - If far from fair, cooldown is longer and the AI shifts motive.
  - Repeated rejected tribute/aid requests reduce trust slightly.
- AI concession ladder:
  - Add small currency/resource sweetener.
  - Shorten access duration.
  - Replace alliance with non-aggression.
  - Replace tribute demand with trade/resource exchange.
  - Offer peace plus temporary non-aggression.
- AI should not haggle in real-time popups; it sends discrete proposals through the existing inbox/log.

## Access And Trespass Intelligence
- Replace blanket trade-pact permission with directional access grants.
- AI evaluates foreign presence by actor intent:
  - Civilian trader/courier with market access: allowed near market corridor.
  - Nomads with seasonal camp access: allowed inside granted camp disc.
  - Ordinary civilian with safe passage: allowed to cross, not harvest/build.
  - Drafted/raiding/attacking actor: hostile regardless of access.
- Trespass warning policy becomes personality-aware:
  - Defensive/martial factions warn sooner and escalate faster.
  - Mercantile factions tolerate traders near markets.
  - High trust grants one extra grace incident.
  - Existing warning → ignored warning → defense queue escalation remains.

## Implementation Changes
- Add `diplomacy_ai.rs` for motive scoring, candidate generation, deal evaluation, acceptance prediction, and offer memory.
- Keep existing `diplomacy.rs` as ledger/proposal/treaty storage; add `DealPackage`, `DealTerm`, active access grants, obligations, and deal ids.
- Add courier obligations for transfer terms; accepted deals create `DealObligation`s, assigned to traders/bureaucrats/idle adults, delivered physically via existing routing/storage patterns.
- Update `trespass.rs` to call access-aware classification.
- Update `ui/diplomacy_panel.rs` to show known contacts only, proposal term summaries, and player-perspective labels: `Fair`, `Risky`, `Bad Deal`.
- Update activity log for received offer, accepted deal, delivery complete, default, access granted/revoked, and trespass warning.

## Test Plan
- Pure AI tests:
  - Hungry faction requests aid or trade, not alliance.
  - Strong martial faction demands tribute only when target fear/imbalance supports it.
  - Mercantile faction proposes market access/resource exchange when surplus/shortage matches.
  - Defensive faction rejects broad safe passage after repeated trespass.
  - Peace offer accepted under fear/cost, rejected when grievance remains high and no concession exists.
- Integration tests:
  - AI proposes only to scouted/visited contacts.
  - AI-to-AI fair deal is accepted and creates courier obligations.
  - Courier delivery completes a resource deal and records trust.
  - Failed delivery defaults the deal and damages trust.
  - Safe passage suppresses civilian trespass warning but not military intrusion.
  - Player receives AI offers in panel plus activity log.

## Assumptions
- “Smart” means utility-driven, memoryful, personality-shaped, and non-omniscient.
- v1 does not transfer plots or raw territory cells.
- AI can estimate target stocks imperfectly from known markets, prior trades, and sightings; exact private storage is only known for self.
- No new crates; reuse Bevy systems, `AHashMap`, existing market/storage/trader/routing patterns.
