# UI (`src/ui/`)

All UI uses `bevy_egui` (not `bevy_ui`, except for specific overlays).

## Player UI

- **Right-click context menu (`orders.rs`):** Three sections — tile actions (Move/Mine/Gather/Dig Down/Deconstruct/Build), entities on tile (Attack / Pick up corpse), ground item stacks (Pick up: Nx Good). `PlayerOrderKind::{PickUpItem, AttackEntity, PickUpCorpse}` route to Scavenge/MilitaryAttack/PickUpCorpse tasks. `TileDisplayQueries` and `RoutingResources` are `SystemParam` bundles to stay under Bevy's 16-param limit.
- **Inspector (`inspector.rs`):** Drop / unequip / equip buttons queue actions via `PendingInspectorAction`; `inspector_action_system` executes after panel render. `valid_equip_slots(good)` in `../simulation/items.rs` maps goods to slots. Knowledge section lists the selected person's Learned/Aware sets and capacity used/total. Each Learned row carries small "Lecture" / "Encode" buttons that fire `InspectorActionKind::HoldLecture` / `EncodeTablet`; each Aware-only row whose tech matches an inventory tablet/book carries a "Read" button (`ReadItem`). An "In progress" sub-section lists `study_progress` entries as `tech: progress/threshold ticks`.
- **Tech panel (`tech_panel.rs`):** Tri-state per tech — ✓ green (chief Learned), ◉ teal (chief Aware only), ◎ yellow (discoverable), ○ gray (locked) — plus a hover row "Complexity: N pt(s) · Members: N/M learned, N aware".
- **Activity log (`activity_log.rs`):** Bottom-right egui panel; `ActivityLogEvent { tick, actor, faction_id, kind }` with kinds `Constructed`, `Crafted`, `TechDiscovered`, `RegionSettled`, `Taught { student_name, tech_name }`, `Read { tech_name }`. Filtered to player faction; capped at 16 entries.
- **Spawn-select (`spawn_select.rs`):** Full-screen biome map with mega-chunk grid overlay; click on habitable cell sets `PendingSpawn`. Ocean/Mountain non-habitable.
- **World-map switcher (`world_map.rs::world_map_system`):** Settled mega-chunks outlined (yellow=player, red=other). Click bookmarks current camera onto the region containing it, then jumps to target's `camera_bookmark`. Tab toggles map.
- **Muster button (`../simulation/military.rs`):** Sets `MusterHuntersRequest.pending`; `apply_muster_hunters_system` (Economy) inserts `Drafted` on every player-faction Hunter, clearing plan/reservations and removing `Carrying`. Player issues rally point separately via right-click. Orthogonal to chief muster (uses `HuntOrder::Hunt::mustered`, not `Drafted`).
