**Loose Resource Stockpile Cleanup**

**Summary**
Fix faction storage hauling as a general loose-item cleanup pipeline. Workers should sweep useful public `GroundItem`s into faction storage, especially harvest spill like grain and seeds, instead of only gathering fresh resources or hauling one edible at a time.

**Implementation Changes**
- Add a loose-stockpile posting pass in `jobs.rs` that scans non-storage `GroundItem`s near faction storage/home and posts `JobKind::Stockpile { resource_id }` for useful resources: food, seeds, materials, fuel, ore, hide, cloth, tools, weapons, armor, shields, and luxuries.
- Reuse the existing `Task::Scavenge -> Task::DepositToFactionStorage` chain. Extend the HTN stockpile dispatchers so a claimed worker can find matching loose items from live `SpatialIndex`/shared sightings, not only current vision.
- Change `item_pickup_system` so scavenge chains followed by `DepositToFactionStorage` pick up as much of the target stack as the worker can carry. Keep hunger-sized pickup only for scavenge chains followed by `Eat`.
- Make `drop_items_at_destination_system` credit stockpile progress for every deposited resource, not just hand-carried wood/stone. Food deposits should credit calories whether the food came from hands or inventory; resource-specific postings should credit exact `ResourceId` matches.
- Preserve economy policy: cleanup postings only appear when the faction’s policy for that resource allows chief labor. Nomadic/packed/posting-disabled factions remain excluded.
- Update `AGENTS.md` with the new loose-resource cleanup behavior.

**Tests**
- Stockpile scavenge of loose grain with low hunger takes a carried stack, not one bite.
- Loose grain and loose grain seed near a settlement get posted, claimed, scavenged, deposited, and removed from the field.
- Generic stockpile completion works for a non-wood resource such as skin/hide.
- Existing eat-scavenge behavior still takes only what hunger needs.
- Run targeted tests plus `cargo test --bin civgame`.

**Assumptions**
- “Useful resources” means public loose items in the included resource classes, not private market ownership or theft handling.
- Market-mode factions keep current policy behavior: no communal cleanup when `chief_allocates_labor` is false.
