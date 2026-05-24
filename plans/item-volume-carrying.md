# Add Item Volume and Capacity-Based Carrying

## Summary
- Add per-unit `volume_ml` to catalog resources and expose it through `ResourceId` / `Item` helpers alongside existing `weight_g`.
- Remove the fixed `HAND_QTY_CAP = 3`; hands, inventory, pack animals, and vehicles will accept quantities based on weight plus volume.
- Preserve the existing `Bulk` idea: `Small` items use a small stack volume cap, while `OneHand` / `TwoHand` bulky stacks may exceed that cap but are bounded by hand slots, weight, and an oversized-load volume cap.
- Faction storage tiles remain unlimited ground piles/rollups.

## Key Changes
- Extend `ResourceDef` with required `volume_ml: u32`; add `ResourceId::unit_volume_ml()`, `Item::unit_volume_ml()`, and `Item::stack_volume_ml(qty)`.
- Add approximate gameplay-tuned volume literals in `assets/data/resources/core.ron`. Use examples like seeds `5ml`, water `500ml`, fruit `300ml`, wood `35L`, stone/ores `8L`, bedroll `12L`, packed yurt `180L`, small/medium cart frames `90L/180L`.
- Add shared capacity helpers for “units that fit by weight and volume” so `EconomicAgent`, `Carrier`, `PackAnimalInventory`, and `VehicleInventory` do not duplicate math.
- Widen material transfer quantities from `u8` to `u32` for `Task::WithdrawMaterial`, `Task::BuyMaterialAtMarket`, typed accessors, and `PersonAI.reserved_qty`.

## Carrying Behavior
- `EconomicAgent` gains small-item and bulky-item volume capacity fields, with helpers for current/free volume. `add_item`, `is_inventory_full`, nomad redistribution, market buys, and inspector actions must reject by both weight and the matching volume bucket.
- `Carrier` keeps two hand slots:
  - `Bulk::Small`: can top up matching small stacks until per-hand small volume or weight is full.
  - `Bulk::OneHand`: one bulky stack per hand, bounded by per-hand oversized volume and weight.
  - `Bulk::TwoHand`: one bulky stack occupying both hands, bounded by combined oversized volume and weight.
- Initial hand defaults: `25kg` per hand, `2L` small volume per hand, `100L` oversized volume per hand. Initial personal inventory defaults: `5kg`, `8L` small volume, `25L` bulky volume.
- Replace `Carrier::is_at_haul_cap()` logic with capacity-aware checks, including an item-aware helper for gather/dig/withdraw paths.
- Update gather, dig, fishing, terraform, scavenge, construction refunds, withdraw-food/material, member transfers, tool stowing, and inspector equip/unequip spill paths to use the new volume-aware capacity APIs and preserve leftovers as ground items.

## Logistics
- HTN haul dispatch should choose `qty = min(remaining blueprint/order need, effective stock, actor carry capacity)` instead of hardcoded `1`, then reserve that exact quantity.
- Market haul buys the same computed quantity, bounded by escrow funds and carry capacity.
- `PackAnimalInventory` gains volume capacity and checks both weight and volume. Defaults: horse `180L`, cow `240L`, pig `80L`, dog `35L`.
- Vehicle stats gain `max_cargo_volume_ml`; cargo bays keep weight payload and add physical volume. Vehicle loading, `capacity_units`, overload checks, rollover context, and UI use both kg and liters.

## UI And Docs
- Inspector and hover panels show inventory/hand/pack/vehicle load as both weight and volume.
- Vehicle designer shows max payload in kg and cargo space in liters.
- Update root `AGENTS.md`, `src/economy/CLAUDE.md`, and `src/simulation/CLAUDE.md` to document volume, small-vs-bulky carry rules, and the removal of the hand quantity cap.

## Test Plan
- Unit tests for catalog parsing, item volume helpers, inventory volume rejection, small-stack hand volume caps, bulky stacks exceeding small caps, and `pickup_capacity` matching `try_pick_up`.
- Regression tests proving small items cannot bypass the small cap by forming a large stack, while bulky items can exceed it only within oversized volume and weight.
- Pack animal and vehicle tests for volume-limited cargo, weight-limited cargo, and overload detection.
- HTN/fixture tests for multi-unit withdraw reservations, multi-unit blueprint delivery, market procurement quantities, and no item loss on overflow.
- Run `cargo test --bin civgame` and `cargo check`.

## Assumptions
- `Bulk::Small` defines small-volume items; `Bulk::OneHand` and `Bulk::TwoHand` define bulky items.
- No new crates are needed.
- Volume affects capacity only; movement speed and energy encumbrance remain unchanged in this pass.
