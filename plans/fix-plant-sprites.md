# Make Plant Sprites Reliably Visible

## Summary
Fix the invisible-plant issue at the fallback sprite layer. The catalog has 55 plant species and all have species folders, but none have `seed.png`, so every seed-stage plant uses `_form_*` fallback art. Several fallback seed, seedling, and harvested sprites are only 5-11 painted pixels, which makes them look missing in-game.

## Key Changes
- Update [plant_sprites.rs](/Users/xiao1/civgame/src/rendering/plant_sprites.rs) fallback templates for `_form_*` seed, seedling, and harvested stages so each has a grounded, high-contrast footprint.
- Keep the existing resolution model: species PNG -> form PNG -> legacy ASCII. Do not duplicate `seed.png` into all species folders.
- Regenerate the `_form_*` PNG assets with the existing ignored generator: `cargo test --bin civgame -- --ignored gen_form_fallback_pngs`.
- Add non-ignored sprite audit tests in `plant_sprites.rs`:
  - every catalog species sprite folder exists;
  - every catalog species and growth stage resolves to either species art or form fallback art;
  - fallback early-stage PNGs meet visibility floors: seed/harvested at least 16 non-transparent pixels and a 4x4 painted bbox, seedling at least 20 non-transparent pixels.
- Update [AGENTS.md](/Users/xiao1/civgame/AGENTS.md) rendering notes with the plant sprite fallback rule and audit expectation.

## Test Plan
- Run `cargo test --bin civgame plant_sprite`.
- Run `cargo test --bin civgame plant_catalog`.
- Run `cargo check`.
- Optional visual check: run `cargo run -- --sandbox` and inspect newly planted or seed-stage plants.

## Assumptions
- “Invisible plants” refers primarily to seed or early fallback-stage plants, not missing mature species art.
- The intended fix is asset/template visibility plus regression coverage, with no new crates and no change to plant gameplay logic.
