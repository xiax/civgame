# Animal Taming, Husbandry, And Draft Work

## Summary

Implement animal domestication as a real simulation layer, not just a `Tamed` marker. Keep `Tamed { owner_faction }` for compatibility, add richer domestic state, generalize taming beyond horses, seed small starting herds for eligible factions, and add husbandry infrastructure so cattle/horses can be used for plowing and cart hauling.

## Key Changes

- Add a species metadata layer in `animals.rs`:
  - `DomesticSpecies::{Horse, Cattle, Pig, Cat, Dog}` and `AnimalUse::{Pack, Mount, Plow, Cart, Companion, Guard}`.
  - `DomesticAnimal { species, owner_faction, training, preferred_home, last_cared_tick }`.
  - `AnimalWorkClaim { worker, use_kind, expires_tick }` to prevent duplicate taming/hitching.
  - Keep `Tamed` as the lightweight ownership component used by existing mount/pack systems.

- Generalize taming:
  - Replace `AgentGoal::TameHorse`, `TameHorseScorer`, `TameWildHorseMethod`, and `htn_tame_horse_dispatch_system` with generic `TameAnimal`.
  - Target horses/cattle/pigs/cats/dogs using one metadata table with per-species tech gates.
  - Validate adjacency every tick in `tame_task_system`; cancel if the animal moves away, dies, is already claimed, or no longer matches the worker’s tech.
  - On successful taming, insert `Tamed`, `DomesticAnimal`, appropriate pack/cargo capacity, and settled/nomad home-follow behavior.
  - If the target has `WildHerdMember`, remove it and update `WildHerdRegistry` so a tamed bloomed herd member is not despawned/restored on collapse.
  - Add `IndexedKind` coverage for all tameable species so spatial scans work for cattle/pigs/cats/dogs, not only horses.

- Add husbandry infrastructure:
  - New build kinds: `AnimalPen`, `Stable`, `FeedTrough`, `HitchingPost`.
  - New organic settlement district/zone: `Husbandry`, placed near Agricultural belts, water, and road frontage but outside dense residential cores.
  - Pens/stables carry `AnimalHousing { faction_id, capacity, species_mask }`; domestic animals pick the nearest valid housing or faction home as fallback.
  - Feed troughs consume stored `grain`; grazing animals can also recover hunger on grass/scrub/pasture tiles.
  - Domestic reproduction preserves ownership when both parents are same-owner domestic animals; offspring inherit `DomesticAnimal` and begin untrained.
  - Add basic manure loop: fed cattle/horses/pigs periodically drop `manure`; farms can use it for a small fertility/yield bonus.

- Add draft implements and work:
  - Add catalog resources/core ids for `ard_plow`, `cart`, and `manure`.
  - Add craft recipes:
    - `Ard Plow`: gated by `ARD_PLOW`, wood + tools.
    - `Ox Cart`: gated by `OX_CART`, wood + tools + skin.
  - Add typed tasks:
    - `HitchAnimal { animal, implement }`
    - `PlowPlot { plot_id, animal, plow }`
    - `CartHaul { source, dest, animal, cart, resource_id, qty }`
  - Plowing requires `ARD_PLOW`, an available plow item, and a trained male cattle animal; it marks the plot as plowed for the current growing year and boosts grain yield/fertility.
  - Cart hauling requires `OX_CART`, a cart item, and a trained horse or cattle animal; it uses a cart cargo capacity larger than hand-carry and applies to construction/material stockpile hauls first.
  - Horses remain mount-first; cattle are preferred for plowing and heavy carts; pigs/cats/dogs do not pull implements.

- Startup seeding:
  - Add `seed_starting_tamed_animals_system` after `spawn_animals` / `spawn_population` and before warmup completion.
  - For every eligible non-SOLO faction:
    - `DOG_DOMESTICATION`: seed 1-2 dogs near home.
    - `ANIMAL_HUSBANDRY`: seed 2 cattle, 2 pigs.
    - `HORSE_TAMING`: seed 2 horses.
  - Scale modestly with founder population, cap small default herds, and include both sexes when possible.
  - Settled factions spawn animals near pens/stables if seeded; otherwise near home. Nomadic factions spawn them with `FollowingBand`.

## Implementation Notes

- Update scheduling so `attach_pack_inventory_system`, husbandry care, work-claim expiry, and domestic follow/home behavior run in deterministic Sequential/Economy slots.
- Extend `animal_sense_system` so domestic animals do not flee owner-faction members, but still react to predators.
- Extend `animal_reproduction_system` to split wild and domestic births; same-owner domestic parents create owned offspring.
- Update `compute_faction_storage_system` so pack/cart cargo contributes to mobile factions, and cart cargo deposits cleanly into normal storage on task completion.
- Update UI hover/order labels to show species, sex, owner, role, training, hunger/thirst, home pen, and current work claim.
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` for the new domestication, husbandry, and draft-work contracts.

## Test Plan

- Taming executor:
  - Succeeds only when adjacent for the required work duration.
  - Cancels if target moves away, dies, is claimed, or lacks tech.
  - Removes `WildHerdMember` and prevents herd-collapse despawn.
  - Supports horse/cattle/pig/cat/dog via the metadata table.

- Startup:
  - Neolithic starts get cattle/pigs when `ANIMAL_HUSBANDRY` is known.
  - Bronze starts also get horses when `HORSE_TAMING` is known.
  - AI and player factions both receive small default herds.
  - Nomadic starts receive following animals, not settled pen assignments.

- Husbandry:
  - Domestic animals select valid pens/stables.
  - Feed trough grain reduces hunger.
  - Domestic offspring inherit ownership.
  - Over-capacity housing leaves excess animals near home without panics.

- Draft work:
  - Male cattle + plow can plow an assigned agricultural plot.
  - Plowed plots increase grain yield.
  - Horse/cattle + cart can move more material than hand hauling.
  - Animal/work claims release on completion, cancellation, death, or timeout.

- Regression:
  - Existing horse mounting still works.
  - Existing nomad pack-animal inventory still works.
  - `cargo test --bin civgame tame animal husbandry cart plow` plus full `cargo test --bin civgame`.

## Assumptions

- “Full husbandry sim” means visible housing, feeding, breeding ownership, manure, and draft labor in v1.
- Starting tamed animals use small default herds for every eligible faction, including AI.
- Plowing is cattle-only in v1, preferring/iring male cattle as bulls/oxen.
- Cart pulling is horse or cattle in v1.
- Dogs become real domestic animals under `DOG_DOMESTICATION`; cats remain companion animals under the same companion-domestication gate unless a later cat-specific tech is added.
