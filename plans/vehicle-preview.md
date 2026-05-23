# Vehicle Designer Preview + Debug Test Drive

## Summary
Add two designer-facing features: a live world-sprite preview of the current vehicle design, and a debug-only “Test Drive” shortcut that spawns the current valid design as a free, permanent real vehicle and enters manual drive mode.

Chosen defaults:
- Preview uses the same world sprite composition as spawned vehicles.
- Test Drive exists only when debug mode is enabled.
- Test Drive bypasses tech/resource/yard requirements, but still requires a valid vehicle grid.
- Spawned test vehicles are permanent normal vehicles after manual drive ends.

## Key Changes
- Extract shared vehicle visual composition from `spawn_vehicle_sprites` into a reusable helper that accepts a `VehicleDesign` plus heading and returns the stock-cart or composed-cell sprite plan.
- Add a non-sim `VehicleDesignerPreview` world entity controlled by the designer UI:
  - It renders with the shared vehicle sprite helper.
  - It updates whenever the working grid, heading, material, module layout, or purpose changes.
  - It does not carry `Vehicle`, does not affect `VehicleOccupancyIndex`, cannot be selected as a vehicle, and is cleaned up when the designer closes or preview is toggled off.
- Add designer controls:
  - “World Preview” toggle.
  - Preview heading/rotate buttons.
  - “Focus Preview” to move the camera to the preview entity.
  - “Test Drive (debug)” button shown only when debug mode is enabled.
- Add `VehicleDebugMode` resource:
  - Defaults to `cfg!(debug_assertions)`.
  - `--sandbox` can leave it enabled through the same resource.
  - Release builds default to disabled, and the Test Drive button is hidden/disabled.
- Add `PlayerCommand::DebugSpawnCustomVehicle { name, grid, purpose, required_animals }`:
  - UI emits this event only for valid, non-empty designs in debug mode.
  - The command registers the design in `VehicleDesignRegistry` with derived tech gates.
  - It spawns a real `Vehicle` immediately via a promoted `spawn_vehicle_at` helper, with `VehicleInventory`, `VehicleCrew`, `VehicleDraft`, and `VehicleHealth`.
  - It does not consume resources, require a `VehicleYard`, or enforce faction tech gates.
- Add spawn-site selection for debug vehicles:
  - Prefer a valid tile near a player VehicleYard.
  - Fallback to player home.
  - Fallback to camera-centered nearby passable tiles.
  - Validate full vehicle footprint, vertical clearance, and occupancy before spawning.
- Add `VehicleManualDriveState { active_vehicle, follow_camera, last_status }`:
  - After debug spawn, set the spawned vehicle active, select/focus it, and enter manual drive.
  - Ending manual drive clears only the control state; the vehicle remains in-world.
- Add manual drive input:
  - `W`/Up: drive one forward step.
  - `A`/Left and `D`/Right: turn in place.
  - `Q`/`E`: forward-left / forward-right diagonal step.
  - `S`/Down: stop/cancel current manual route.
  - `Esc`: exit manual drive.
  - Ignore manual drive input while egui wants keyboard input.
- Manual drive applies real vehicle movement:
  - A new helper builds one-step `VehiclePathFollow` paths from current pose.
  - It uses the same footprint, clearance, occupancy, terrain passability, speed, and rollover systems as normal vehicle movement.
  - It does not issue a new step while the vehicle already has a `VehiclePathFollow`.
  - Camera panning should be suppressed while manual drive is active so WASD/arrow keys steer the vehicle instead of the camera.

## Interfaces
- New or changed types:
  - `VehicleDebugMode`
  - `VehicleDesignerPreview`
  - `VehicleDesignerPreviewVisual`
  - `VehicleManualDriveState`
  - `PlayerCommand::DebugSpawnCustomVehicle`
- Promoted/shared helpers:
  - `spawn_vehicle_at(...)`
  - `vehicle_sprite_plan(...)`
  - `vehicle_pose_fits(...)`
  - `manual_drive_step(...)`
- Docs to update:
  - Vehicle designer notes in `src/ui/CLAUDE.md`
  - Vehicle system notes in `src/simulation/CLAUDE.md`
  - Root `AGENTS.md` vehicle/debug UI notes if the behavior summary there changes.

## Test Plan
- Unit tests:
  - Preview sprite plan matches the existing spawned-vehicle composed-cell layout.
  - Heading changes alter preview/spawn sprite cell offsets consistently.
  - Preview entities do not enter `VehicleOccupancyIndex`.
  - Debug spawn registers a custom design and spawns a real `Vehicle` without consuming storage.
  - Debug spawn is rejected when `VehicleDebugMode` is disabled.
  - Spawn-site finder rejects blocked, low-clearance, occupied, and impassable footprints.
  - Manual forward/turn/diagonal controls create valid one-step `VehiclePathFollow` paths.
  - Manual controls reject blocked steps without moving the vehicle.
  - Manual movement still feeds existing rollover logic for unstable designs.
- Commands:
  - `cargo test --bin civgame vehicle -- --quiet`
  - `cargo test --bin civgame`
  - `cargo check`
- Manual QA:
  - `cargo run`
  - Open Vehicles, build a valid custom design, confirm the world preview updates while editing.
  - Click Test Drive in debug mode, confirm a permanent vehicle appears for free.
  - Drive with manual keys over road/rough terrain and confirm speed/turn behavior feels like normal vehicle movement.
  - Confirm release mode hides or disables Test Drive.
