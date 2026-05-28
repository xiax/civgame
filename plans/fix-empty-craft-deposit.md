# Fix Empty Craft Deposit Stall

## Context

When `craft_order_system` finishes a satisfied order (`src/simulation/crafting.rs:1342`), it grants the recipe output to the lead worker by calling `agent.add_item(output_item, recipe.output_qty)` at `crafting.rs:1440` and discards the return value. But `EconomicAgent::add_item` returns the qty that didn't fit (`src/economy/agent.rs:135` — weight cap AND per-bulk volume cap), so heavy outputs like Leather Armor or Cart Wheel can silently drop on the floor. The worker then walks the queued `DepositToFactionStorage` leg to storage with nothing in hand or inventory.

Second, independent bug at the same site: `WorkOnSatisfiedCraftOrderMethod::expand` (`htn.rs:10418`) emits `[WorkOnCraftOrder, DepositToFactionStorage]`, but the handoff at `crafting.rs:1481-1539` runs for **every** entity in `completed_agents` (lead worker + non-lead workers). Only the lead is granted output (`crafting.rs:1349-1351`); the rest walk to storage carrying nothing. (Haulers end on `HaulToCraftOrder`, not `DepositToFactionStorage`, so they're already fine — only non-lead workers in a multi-worker order are affected.)

Net effect: workers appear stuck on deposit chains, hands empty, no work done. Output items vanish from the world.

## Approach

Three changes, all in `craft_order_system`'s completion path (`src/simulation/crafting.rs`):

### 1. Lead-worker output: hands first, inventory next, ground last

Replace the bare `agent.add_item(output_item, recipe.output_qty);` at `crafting.rs:1440` with a three-tier cascade modeled on `withdraw_material_task_system`'s heavy-good pattern:

1. `carrier.try_pick_up(output_item, remaining)` — picks up to `pickup_capacity(item)` into hands. Mirrors the documented convention (`economy/CLAUDE.md` → Carrying: "Withdraw routes through hands first … Stone weighs 5000g (= entire inventory cap), so withdraws offer `Carrier::try_pick_up` first."). Outputs can be bulky (Two-Hand cart parts; One-Hand armor); hands hold one oversized load each via `HAND_OVERSIZED_VOLUME_ML`.
2. `agent.add_item(output_item, remaining)` — capture the returned leftover.
3. Any final leftover spawns at `order.anchor_tile` via `items::spawn_or_merge_ground_item_full(commands, ground_item_q, spatial_index, item, qty, anchor_tile, surface_z, /*owner_household=*/None, /*owner_faction=*/Some(faction_id), clock.tick)`.

`order.anchor_tile` must be snapshotted **before** `commands.entity(order_entity).despawn_recursive()` at `crafting.rs:1362`. Store it alongside `recipe_id`/`tech_payload` in the per-completion record (`output_grants`).

Item identity (quality, material, `tech_payload`) is already constructed at `crafting.rs:1430-1439`; the spill uses the same `Item` so clay tablets and books preserve their tech payload at ground level.

`Carrier::try_pick_up` requires the appropriate `Bulk` slot occupancy (TwoHand needs both hands). At craft completion the worker may still be holding tools or ingredients — partial pickup is fine, the cascade handles overflow.

### 2. Job credit stays unconditional

Output is always produced (item lands hands, inventory, or ground), so `order_completion_credits.push(...)` at `crafting.rs:1351` is unchanged. The lead worker's `JobClaim` credit doesn't depend on the item reaching storage — that's the deposit executor's separate path (`drop_items_at_destination_system` → "Generalised deposit credit"), which credits any matching `Stockpile { resource_id }` claim on actual deposit.

### 3. Gate the deposit promotion on actually carrying the resource

At `crafting.rs:1497-1539`, before routing the worker via `assign_task_with_routing(... TaskKind::DepositResource ...)`, verify the worker actually holds the deposit task's `resource_id`. Catches:

- Non-lead workers in multi-worker orders who never received output.
- Defensive: lead's add_item + hands + ground spawn all failed (won't happen — ground spawn is unbounded — but cheap).

Implementation: extract the `resource_id` from `aq.current` (`Task::DepositToFactionStorage { resource_id, .. }`), then check `carrier.qty_of(resource_id) + agent.qty_of(resource_id) > 0`. On false, call `aq.cancel_chain(&mut ai)` and `ai.active_method = None` (the latter prevents `htn_method_completion_system` writing a phantom `Success` next tick — same treatment the no-storage-tile fallback at `crafting.rs:1537` should apply).

Per `simulation/CLAUDE.md` → Task failure protocol, no `record_*_failure` is needed — the chain was a structural over-emit, not a worker-picked target loss. `Interrupted` semantics.

## Critical files

- `src/simulation/crafting.rs` — `craft_order_system`, completion block at lines 1342-1539. All three changes land here.
- `src/economy/agent.rs:135` — `add_item` return-value semantics (unchanged; just consumed now).
- `src/simulation/items.rs` — reuse `spawn_or_merge_ground_item_full` (public faction-property spill — the `_owned` household variant is not appropriate).
- `src/simulation/CLAUDE.md` — alongside the "CraftOrders + jobs" paragraph, add one line: "Craft output is granted hands → inventory → spilled at order anchor; non-lead workers in multi-worker orders cancel their trailing `DepositToFactionStorage` instead of walking empty."

## Tests

Add to `src/simulation/crafting.rs` test module (next to existing craft tests):

1. **`craft_output_spills_to_ground_when_inventory_full`** — drive a satisfied craft order for a heavy output (Leather Armor or Cart Wheel) on a lead worker whose `EconomicAgent` and `Carrier` are pre-filled to capacity with junk. Tick once after `work_progress == work_ticks`. Assert: a `GroundItem` matching the output (resource_id, qty, quality, material) exists at `order.anchor_tile`. Assert: the `DepositToFactionStorage` chain is either canceled (no carry) or routed (partial carry). Use `TestSim::spawn_person(...).add_inventory(...)` from `test_fixture.rs`.

2. **`non_lead_worker_cancels_empty_deposit_in_multi_worker_order`** — spawn a satisfied craft order with two workers registered in `order_workers`; tick once. Assert: lead worker has `aq.current == Task::WalkTo` toward storage (or `DepositResource` queued) and is carrying/inventorying the output. Non-lead worker has `aq.current == Task::Idle` (chain canceled), not routed anywhere.

3. **`craft_output_preserves_item_metadata_on_spill`** — encode a Clay Tablet recipe (output carries `tech_payload`); force inventory-full + hands-full; tick. Assert spawned `GroundItem`'s `Item.tech_payload == Some(expected_tech_id)` and `quality` / `material` match `recipe.effective_quality` + `recipe.output_material`. Guards the spill path against losing item structure.

Run: `cargo test --bin civgame craft` and `cargo check`.

## Verification (in-game)

- `cargo run` with a Bronze+ faction queueing an armor or cart recipe; watch the activity log for `Crafted` events without a follow-up empty walk-to-storage stall.
- Inspector on lead worker post-craft: hands or inventory must show the output before deposit. Non-lead worker on the same order shows `Idle` within one or two ticks.
- If hands and inventory genuinely can't fit (pre-stack the inventory), expect a `GroundItem` sprite at the workbench anchor.

## Out of scope

- Order-time chain re-design (emit deposit only for the designated lead): would require `WorkOnSatisfiedCraftOrderMethod` to know which worker is lead at dispatch time. The completion-time cancel gate gets the same outcome with one branch.
- Output sizing: workers can still produce a unit they can't personally transport. Ground spill is the intended escape valve; multi-trip hauling is a separate plan.
- Hauler chains end at `HaulToCraftOrder` (no trailing deposit), so they need no change.
