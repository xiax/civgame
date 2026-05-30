//! Per-species plant sprite loading + lookup (Phase 6 of biome-native plants
//! follow-ups). Mirrors `pixel_art::AnimalTextures`: walk the catalog at
//! startup, probe `assets/textures/plants/<folder>/<stage>_<variant>.png`,
//! cache `Handle<Image>` per `(species, stage, variant)`. Missing PNGs fall
//! through to a per-`PlantForm` shared fallback layer
//! (`assets/textures/plants/_form_<form>/...`), then to the legacy ASCII
//! sprites in `EntityTextures` at resolve time.
//!
//! The resolver returns `None` when no per-species or per-form PNG slot is
//! present; the caller (`entity_sprites::resolve_plant_sprite`) then routes
//! to legacy ASCII. This file does not own the legacy fallback handles.

use crate::collections::AHashMap;
use bevy::prelude::*;

use crate::simulation::plant_catalog::{PlantCatalog, PlantForm, PlantSpeciesId};
use crate::simulation::plants::GrowthStage;

/// Stage axis size — matches `GrowthStage as u8` count.
const STAGE_COUNT: usize = 5;

/// One stage's variant slots. `variant_count == 0` means "no PNGs authored
/// for this (species, stage)"; resolver should fall through to form
/// fallback or legacy ASCII. Variants beyond `variant_count` reuse slot 0.
#[derive(Default, Clone)]
pub struct PlantStageSlot {
    pub variants: [Option<Handle<Image>>; 3],
    pub variant_count: u8,
}

impl PlantStageSlot {
    pub fn handle_for(&self, variant: u8) -> Option<Handle<Image>> {
        if self.variant_count == 0 {
            return None;
        }
        let idx = (variant as usize) % (self.variant_count as usize);
        self.variants[idx].clone()
    }
}

#[derive(Resource, Default)]
pub struct PlantSpriteSet {
    by_species: AHashMap<PlantSpeciesId, [PlantStageSlot; STAGE_COUNT]>,
    by_form: AHashMap<PlantForm, [PlantStageSlot; STAGE_COUNT]>,
}

impl PlantSpriteSet {
    pub fn lookup_species(
        &self,
        species: PlantSpeciesId,
        stage: GrowthStage,
    ) -> Option<&PlantStageSlot> {
        let slots = self.by_species.get(&species)?;
        let slot = &slots[stage as usize];
        if slot.variant_count == 0 {
            None
        } else {
            Some(slot)
        }
    }

    pub fn lookup_form(&self, form: PlantForm, stage: GrowthStage) -> Option<&PlantStageSlot> {
        let slots = self.by_form.get(&form)?;
        let slot = &slots[stage as usize];
        if slot.variant_count == 0 {
            None
        } else {
            Some(slot)
        }
    }
}

/// Stage axis: how many variants we probe per stage and the filename stem.
/// Seed/Harvested are single-variant by design (sprout/stubble doesn't
/// warrant authoring 3 distinct silhouettes).
const STAGE_LAYOUT: [(GrowthStage, &str, u8); STAGE_COUNT] = [
    (GrowthStage::Seed, "seed", 1),
    (GrowthStage::Seedling, "seedling", 3),
    (GrowthStage::Harvested, "harvested", 1),
    (GrowthStage::Mature, "mature", 3),
    (GrowthStage::Overripe, "overripe", 3),
];

const ASSET_ROOT_REL: &str = "textures/plants";
const ASSET_ROOT_FS: &str = "assets/textures/plants";

/// Probe `assets/textures/plants/<folder>/<stem>[_<variant>].png` for each
/// `(species, stage, variant)` and `_form_<form>/...` for the form fallback
/// layer. Files that don't exist on disk are skipped so the resolver can
/// route to a higher fallback tier — Bevy's `AssetServer::load` would
/// happily return a handle for a missing path and log warnings on every
/// frame, which is exactly what we don't want here.
pub fn load_plant_sprites(asset_server: &AssetServer, catalog: &PlantCatalog) -> PlantSpriteSet {
    let mut set = PlantSpriteSet::default();

    for def in catalog.iter() {
        let folder = def.sprite_folder();
        let species_variant_cap = def.sprite_variants();
        let mut stage_slots: [PlantStageSlot; STAGE_COUNT] = Default::default();
        let mut any_loaded = false;

        for (idx, (_stage, stem, max_variants)) in STAGE_LAYOUT.iter().enumerate() {
            let effective_max = (*max_variants).min(species_variant_cap).max(1);
            let mut slot = PlantStageSlot::default();
            for v in 0..effective_max {
                let asset_path = sprite_asset_path(folder, stem, *max_variants, v);
                let fs_path = format!("{ASSET_ROOT_FS}/{folder}/{}", file_name(stem, *max_variants, v));
                if std::path::Path::new(&fs_path).exists() {
                    slot.variants[v as usize] = Some(asset_server.load(&asset_path));
                    slot.variant_count = slot.variant_count.max(v + 1);
                    any_loaded = true;
                }
            }
            // If only some variants were authored, fill gaps with slot 0
            // so `handle_for(variant)` mod-indexing always lands somewhere
            // sensible.
            if slot.variant_count > 0 && slot.variant_count < 3 {
                let first = slot.variants[0].clone();
                for v in (slot.variant_count as usize)..3 {
                    if slot.variants[v].is_none() {
                        slot.variants[v] = first.clone();
                    }
                }
            }
            stage_slots[idx] = slot;
        }
        if any_loaded {
            set.by_species.insert(def.id, stage_slots);
        }
    }

    for form in [
        PlantForm::Grass,
        PlantForm::Forb,
        PlantForm::Shrub,
        PlantForm::Vine,
        PlantForm::Tree,
        PlantForm::Aquatic,
        PlantForm::Cactus,
        PlantForm::Tuber,
    ] {
        let folder = format!("_form_{}", form_slug(form));
        let mut stage_slots: [PlantStageSlot; STAGE_COUNT] = Default::default();
        let mut any_loaded = false;
        for (idx, (_stage, stem, max_variants)) in STAGE_LAYOUT.iter().enumerate() {
            let mut slot = PlantStageSlot::default();
            for v in 0..*max_variants {
                let asset_path = sprite_asset_path(&folder, stem, *max_variants, v);
                let fs_path = format!("{ASSET_ROOT_FS}/{folder}/{}", file_name(stem, *max_variants, v));
                if std::path::Path::new(&fs_path).exists() {
                    slot.variants[v as usize] = Some(asset_server.load(&asset_path));
                    slot.variant_count = slot.variant_count.max(v + 1);
                    any_loaded = true;
                }
            }
            if slot.variant_count > 0 && slot.variant_count < 3 {
                let first = slot.variants[0].clone();
                for v in (slot.variant_count as usize)..3 {
                    if slot.variants[v].is_none() {
                        slot.variants[v] = first.clone();
                    }
                }
            }
            stage_slots[idx] = slot;
        }
        if any_loaded {
            set.by_form.insert(form, stage_slots);
        }
    }

    set
}

fn form_slug(form: PlantForm) -> &'static str {
    match form {
        PlantForm::Grass => "grass",
        PlantForm::Forb => "forb",
        PlantForm::Shrub => "shrub",
        PlantForm::Vine => "vine",
        PlantForm::Tree => "tree",
        PlantForm::Aquatic => "aquatic",
        PlantForm::Cactus => "cactus",
        PlantForm::Tuber => "tuber",
    }
}

fn file_name(stem: &str, max_variants: u8, variant: u8) -> String {
    if max_variants == 1 {
        format!("{stem}.png")
    } else {
        format!("{stem}_{}.png", variant)
    }
}

fn sprite_asset_path(folder: &str, stem: &str, max_variants: u8, variant: u8) -> String {
    format!("{ASSET_ROOT_REL}/{folder}/{}", file_name(stem, max_variants, variant))
}

pub fn setup_plant_sprites(asset_server: Res<AssetServer>, mut set: ResMut<PlantSpriteSet>) {
    let catalog = crate::simulation::plant_catalog::catalog();
    *set = load_plant_sprites(&asset_server, catalog);
}

// ---------------------------------------------------------------------------
// PNG generator — one-shot author of the per-form fallback sprite set.
// Run via `cargo test --bin civgame -- --ignored gen_form_fallback_pngs`.
// Templates draw against `pixel_art::WARM_PALETTE`.
//
// Sizes per form (revised for realistic vertical scale):
//   Grass / Forb / Vine / Tuber : 16×16  (unchanged)
//   Shrub                       : 24×24
//   Cactus                      : 16×32
//   Aquatic                     : 16×24  (reed-sized)
//   Tree                        : 48×80  (Medium tier baseline)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod gen {
    use super::*;
    use crate::rendering::pixel_art::{PixelColor, WARM_PALETTE};
    use std::fs;
    use std::path::PathBuf;

    fn lookup(ch: char) -> PixelColor {
        for (c, col) in WARM_PALETTE {
            if *c == ch {
                return *col;
            }
        }
        PixelColor::new(0, 0, 0, 0)
    }

    // ── tiny ASCII canvas with rect / ellipse fills ────────────────────

    struct Canvas {
        w: usize,
        h: usize,
        pixels: Vec<Vec<char>>,
    }

    impl Canvas {
        fn new(w: usize, h: usize) -> Self {
            Self { w, h, pixels: vec![vec!['.'; w]; h] }
        }

        fn set(&mut self, x: i32, y: i32, c: char) {
            if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                self.pixels[y as usize][x as usize] = c;
            }
        }

        fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: char) {
            for dy in 0..h {
                for dx in 0..w {
                    self.set(x + dx, y + dy, c);
                }
            }
        }

        /// Filled ellipse; inner ~65% of radius gets `fill`, outer ring gets
        /// `outline` for a tiny bit of shading without authoring two passes.
        fn fill_ellipse(&mut self, cx: i32, cy: i32, rx: i32, ry: i32, fill: char, outline: char) {
            let rxf = rx.max(1) as f32;
            let ryf = ry.max(1) as f32;
            for dy in -ry..=ry {
                for dx in -rx..=rx {
                    let nx = dx as f32 / rxf;
                    let ny = dy as f32 / ryf;
                    let d = nx * nx + ny * ny;
                    if d <= 1.0 {
                        let c = if d > 0.62 { outline } else { fill };
                        self.set(cx + dx, cy + dy, c);
                    }
                }
            }
        }

        fn into_rows(self) -> Vec<String> {
            self.pixels
                .into_iter()
                .map(|r| r.into_iter().collect())
                .collect()
        }
    }

    fn from_static(rows: &[&'static str]) -> Vec<String> {
        rows.iter().map(|s| s.to_string()).collect()
    }

    /// Shared 16×16 seed sprite for the four small static forms (grass / forb
    /// / vine / tuber). A bright cotyledon sprout on a dark soil mound —
    /// bottom-anchored and high-contrast so a freshly-sown seed reads against
    /// brown dirt instead of vanishing (the old 8px speck did). At Seed stage
    /// every form looks alike, so one motif is correct *and* DRY.
    fn seed_sprout_16() -> Vec<String> {
        from_static(&[
            "................", "................", "................",
            "................", "................", "................",
            "................", "................", "................",
            "................", "................", "......M..M......",
            ".....MLmmLM.....", "......mGm.......", ".....sSGSs......",
            "....sSSSSSs.....",
        ])
    }

    // ── 16×16 templates (unchanged forms) ──────────────────────────────

    fn grass_template(stem: &str) -> Vec<String> {
        match stem {
            "seed" => seed_sprout_16(),
            "seedling" => from_static(&[
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                ".....M.M.M......",
                ".....MLMLM......",
                "......MGM.......",
                "......mGm.......",
                ".....sSGSs......",
                "....sSSSSSs.....",
            ]),
            "harvested" => from_static(&[
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "....b.b.b.......",
                "....b.b.b.......",
                "...sbsbsbs......",
                "..sSSSSSSSs.....",
            ]),
            "mature" => from_static(&[
                "................",
                "................",
                "................",
                "................",
                "......y.........",
                ".....yoy........",
                "......yo........",
                ".....yoyo.......",
                "......yo........",
                "......yM........",
                "......My........",
                "......M.........",
                ".....MMM........",
                "......M.........",
                "......M.........",
                "................",
            ]),
            "overripe" => from_static(&[
                "................",
                "................",
                "................",
                "................",
                "......B.........",
                ".....bBb........",
                "......Bb........",
                ".....bBbb.......",
                "......Bb........",
                "......BB........",
                "......BB........",
                "......B.........",
                ".....BBB........",
                "......B.........",
                "......B.........",
                "................",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn forb_template(stem: &str) -> Vec<String> {
        match stem {
            "seed" => seed_sprout_16(),
            "seedling" => from_static(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "......y.........", ".....MGM........", "...M.MGM.M......",
                "..MLMMGMMLM.....", "...M.MGM.M......", ".....sSs........",
                "....sSSSs.......",
            ]),
            "harvested" => from_static(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "....b...b.......", "....b...b.......", "...sbs.sbs......",
                "..sSSSSSSSs.....",
            ]),
            "mature" => from_static(&[
                "................", "................", "................",
                "................", "......r.........", ".....rRr........",
                "......o.........", "....G.M.G.......", ".....GMG........",
                "......M.........", ".....MGM........", "......G.........",
                "......G.........", ".....GgG........", "......g.........",
                "................",
            ]),
            "overripe" => from_static(&[
                "................", "................", "................",
                "................", "......b.........", ".....BbB........",
                "......s.........", "....b.b.b.......", ".....bsb........",
                "......b.........", ".....bbb........", "......b.........",
                "......b.........", ".....bbb........", "......d.........",
                "................",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn vine_template(stem: &str) -> Vec<String> {
        match stem {
            "seed" => seed_sprout_16(),
            "seedling" => from_static(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "......M.........",
                ".....MGM........", "...M.MGM........", "...MLMGM.M......",
                "...sSSGSSs......",
            ]),
            "harvested" => from_static(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "...G.G.G.G......",
                "....GG.GG.......", "...G.GGG.G......", "....G...G.......",
                "...G.....G......", "...G.....G......", "...b.....b......",
                "................",
            ]),
            "mature" => from_static(&[
                "................", "................", "....M...M.......",
                "...MGM.MGM......", "....MG.GM.......", "...G.MGM.G......",
                "..MG.MGM.GM.....", ".G..MGGGM..G....", "..G.MGGGM.G.....",
                "...M.GGG.M......", "..MG.GMG.GM.....", "...M.MGM.M......",
                "....MGGGM.......", ".....MGM........", "......b.........",
                "................",
            ]),
            "overripe" => from_static(&[
                "................", "................", "....b...b.......",
                "...bGb.bGb......", "....bG.Gb.......", "...G.bGb.G......",
                "..bG.bGb.Gb.....", ".G..bGGGb..G....", "..G.bGGGb.G.....",
                "...b.GGG.b......", "..bG.GbG.Gb.....", "...b.bGb.b......",
                "....bGGGb.......", ".....bGb........", "......d.........",
                "................",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn tuber_template(stem: &str) -> Vec<String> {
        match stem {
            "seed" => seed_sprout_16(),
            "seedling" => from_static(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......M.........", ".....MGM........",
                "....MLGLM.......", ".....MGM........", "....sSGSs.......",
                "...sSSSSSs......",
            ]),
            "harvested" => from_static(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                ".....d.d........", "....s.s.s.......", "...sSdSdSs......",
                "..sSSSSSSSs.....",
            ]),
            "mature" => from_static(&[
                "................", "................", "................",
                "................", ".....MGM........", "....MGMGM.......",
                "...MGGGMGM......", "....MGMGM.......", ".....MGM........",
                "......G.........", "......G.........", ".....bGb........",
                "....bBBBb.......", "...bBBeBBb......", "....bBBBb.......",
                ".....bbb........",
            ]),
            "overripe" => from_static(&[
                "................", "................", "................",
                "................", ".....bGb........", "....bGbGb.......",
                "...bGGGbGb......", "....bGbGb.......", ".....bGb........",
                "......b.........", "......b.........", ".....bbb........",
                "....BBBBB.......", "...BBeeBBB......", "....BBBBB.......",
                ".....BBB........",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    // ── 24×24 shrub ────────────────────────────────────────────────────

    fn shrub_template(stem: &str) -> Vec<String> {
        let (w, h) = (24, 24);
        let mut c = Canvas::new(w, h);
        match stem {
            "seed" => {
                // soil mound
                c.fill_rect(8, 23, 8, 1, 'S');
                c.fill_rect(9, 22, 6, 1, 's');
                // sprout stem
                for y in 18..22 { c.set(11, y, 'G'); c.set(12, y, 'G'); }
                // bright cotyledon leaves
                c.set(9, 19, 'M'); c.set(10, 19, 'M');
                c.set(13, 19, 'M'); c.set(14, 19, 'M');
                c.set(10, 18, 'L'); c.set(13, 18, 'L');
                c.set(11, 17, 'M'); c.set(12, 17, 'M');
            }
            "seedling" => {
                c.fill_ellipse(11, 17, 4, 3, 'M', 'm');
                for y in 17..23 {
                    c.set(11, y, 'G');
                    c.set(12, y, 'G');
                }
                c.set(11, 23, 'b');
                c.set(12, 23, 'b');
            }
            "harvested" => {
                // bare twiggy bush — outline only, no berries / leaves
                c.fill_ellipse(11, 14, 9, 7, '.', 'd');
                // a few brown remnants
                c.set(7, 11, 'd'); c.set(15, 11, 'd');
                c.set(5, 14, 'd'); c.set(17, 14, 'd');
                c.set(8, 16, 'd'); c.set(14, 16, 'd');
                // base + stub stems
                for y in 17..23 { c.set(11, y, 'b'); c.set(12, y, 'b'); }
                c.set(11, 23, 'd'); c.set(12, 23, 'd');
            }
            "mature" => {
                // lush bush with berries
                c.fill_ellipse(11, 13, 11, 9, 'G', 'm');
                c.fill_ellipse(6, 11, 5, 4, 'M', 'G');
                c.fill_ellipse(16, 11, 5, 4, 'M', 'G');
                c.fill_ellipse(11, 7, 5, 4, 'M', 'G');
                // berries scattered
                c.set(7, 13, 'r'); c.set(15, 13, 'r'); c.set(11, 11, 'r');
                c.set(9, 16, 'r'); c.set(13, 16, 'r'); c.set(5, 14, 'r');
                c.set(17, 14, 'r');
                // base
                for y in 19..23 { c.set(11, y, 'b'); c.set(12, y, 'b'); }
                c.set(11, 23, 'd'); c.set(12, 23, 'd');
            }
            "overripe" => {
                // dying leaves, fallen berries
                c.fill_ellipse(11, 13, 11, 9, 'b', 'd');
                c.fill_ellipse(6, 11, 5, 4, 'b', 'd');
                c.fill_ellipse(16, 11, 5, 4, 'b', 'd');
                c.fill_ellipse(11, 7, 5, 4, 'b', 'd');
                // a few dim spots of remaining colour
                c.set(7, 13, 'R'); c.set(15, 13, 'R');
                for y in 19..23 { c.set(11, y, 'b'); c.set(12, y, 'b'); }
                c.set(11, 23, 'd'); c.set(12, 23, 'd');
                // fallen berries on ground
                c.set(4, 23, 'r'); c.set(18, 23, 'r');
            }
            _ => panic!("unknown stem {stem}"),
        }
        c.into_rows()
    }

    // ── 16×32 cactus ───────────────────────────────────────────────────

    fn cactus_template(stem: &str) -> Vec<String> {
        let (w, h) = (16, 32);
        let mut c = Canvas::new(w, h);
        match stem {
            "seed" => {
                // sandy bed
                c.fill_rect(4, 31, 8, 1, 'e');
                c.fill_rect(5, 30, 6, 1, 'E');
                // small green succulent nub
                c.fill_rect(7, 27, 2, 3, 'G');
                c.set(6, 28, 'M'); c.set(9, 28, 'M');
                c.set(7, 26, 'M'); c.set(8, 26, 'M');
            }
            "seedling" => {
                c.fill_rect(7, 23, 2, 7, 'G');
                c.set(7, 22, 'G'); c.set(8, 22, 'G');
                c.fill_rect(4, 30, 8, 1, 'E');
                c.fill_rect(4, 31, 8, 1, 'e');
            }
            "harvested" => {
                // damaged stub of a cut cactus
                c.fill_rect(7, 25, 2, 5, 'b');
                c.set(6, 25, 'd'); c.set(9, 25, 'd');
                c.fill_rect(4, 30, 8, 1, 'E');
                c.fill_rect(4, 31, 8, 1, 'e');
            }
            "mature" => {
                // tall saguaro with two arms
                c.fill_rect(6, 5, 4, 25, 'G');
                // shadow rib
                for y in 5..30 { c.set(6, y, 'g'); c.set(9, y, 'g'); }
                // left arm
                c.fill_rect(3, 13, 3, 2, 'G');
                c.set(3, 13, 'g');
                c.fill_rect(3, 11, 2, 4, 'G');
                c.set(3, 11, 'g');
                // right arm
                c.fill_rect(10, 16, 3, 2, 'G');
                c.set(12, 16, 'g');
                c.fill_rect(11, 14, 2, 4, 'G');
                c.set(12, 14, 'g');
                // crown highlight
                c.set(7, 5, 'M'); c.set(8, 5, 'M');
                // flowers (optional)
                c.set(7, 4, 'y'); c.set(8, 4, 'y');
                c.fill_rect(4, 30, 8, 1, 'E');
                c.fill_rect(4, 31, 8, 1, 'e');
            }
            "overripe" => {
                // wilted, drying out
                c.fill_rect(6, 5, 4, 25, 'b');
                for y in 5..30 { c.set(6, y, 'd'); c.set(9, y, 'd'); }
                c.fill_rect(3, 13, 3, 2, 'b');
                c.fill_rect(3, 11, 2, 4, 'b');
                c.fill_rect(10, 16, 3, 2, 'b');
                c.fill_rect(11, 14, 2, 4, 'b');
                // dropped fruit
                c.set(5, 30, 'R'); c.set(10, 30, 'R');
                c.fill_rect(4, 30, 8, 1, 'E');
                c.fill_rect(4, 31, 8, 1, 'e');
            }
            _ => panic!("unknown stem {stem}"),
        }
        c.into_rows()
    }

    // ── 16×24 aquatic reeds ────────────────────────────────────────────

    fn aquatic_template(stem: &str) -> Vec<String> {
        let (w, h) = (16, 24);
        let mut c = Canvas::new(w, h);
        // water surface at bottom 5 rows
        let water_top: i32 = 19;
        for y in water_top..(h as i32) {
            for x in 0..(w as i32) {
                let ch = if (x + y) % 4 == 0 { 'I' } else { 'i' };
                c.set(x, y, ch);
            }
        }
        // foam line
        c.set(2, water_top, 'H'); c.set(7, water_top + 1, 'H');
        c.set(12, water_top, 'H'); c.set(5, water_top + 2, 'H');
        c.set(10, water_top + 3, 'H');
        match stem {
            "seed" => {
                // green shoot rising from the water
                for y in 14..19 { c.set(7, y, 'G'); c.set(8, y, 'G'); }
                c.set(6, 15, 'M'); c.set(9, 15, 'M');
                c.set(7, 13, 'M'); c.set(8, 13, 'M');
                // cream seed husks at the waterline (pop against blue)
                c.set(6, 18, 'W'); c.set(9, 18, 'W');
            }
            "seedling" => {
                for y in 16..19 { c.set(7, y, 'G'); c.set(8, y, 'G'); }
                c.set(6, 15, 'G'); c.set(9, 15, 'G');
            }
            "harvested" => {
                c.set(6, 18, 'b'); c.set(7, 18, 'b');
                c.set(8, 18, 'b'); c.set(9, 18, 'b');
                c.set(7, 17, 'b'); c.set(8, 17, 'b');
            }
            "mature" => {
                // three reed stems with seed heads
                for y in 3..19 { c.set(5, y, 'G'); }
                for y in 1..19 { c.set(8, y, 'G'); c.set(9, y, 'G'); }
                for y in 4..19 { c.set(12, y, 'G'); }
                // cattail seed heads
                c.set(5, 2, 'b'); c.set(5, 1, 'b'); c.set(5, 0, 'b');
                c.fill_rect(7, 1, 3, 4, 'b');
                c.set(7, 0, 'd'); c.set(9, 0, 'd');
                c.set(12, 3, 'b'); c.set(12, 2, 'b'); c.set(12, 1, 'b');
                // tip leaves
                c.set(4, 6, 'M'); c.set(13, 6, 'M');
                c.set(6, 9, 'M'); c.set(11, 11, 'M');
            }
            "overripe" => {
                for y in 3..19 { c.set(5, y, 'd'); }
                for y in 1..19 { c.set(8, y, 'd'); c.set(9, y, 'd'); }
                for y in 4..19 { c.set(12, y, 'd'); }
                // burst heads — fluff
                c.set(7, 0, 'T'); c.set(8, 0, 'T'); c.set(9, 0, 'T'); c.set(10, 0, 'T');
                c.set(8, 5, 'T'); c.set(9, 5, 'T');
                c.set(5, 1, 'T'); c.set(12, 1, 'T');
            }
            _ => panic!("unknown stem {stem}"),
        }
        c.into_rows()
    }

    // ── 48×80 tree (Medium-tier baseline) ──────────────────────────────

    fn tree_template(stem: &str) -> Vec<String> {
        let (w, h) = (48, 80);
        let mut c = Canvas::new(w, h);
        match stem {
            "seed" => {
                // soil mound
                c.fill_rect(18, 79, 12, 1, 'S');
                c.fill_rect(20, 78, 8, 1, 's');
                // sprout stem
                for y in 72..78 { c.set(23, y, 'G'); c.set(24, y, 'G'); }
                // bright cotyledon leaves
                c.fill_ellipse(20, 73, 3, 2, 'M', 'm');
                c.fill_ellipse(27, 73, 3, 2, 'M', 'm');
                c.set(23, 71, 'M'); c.set(24, 71, 'M');
                // pale seed husks at the base
                c.set(22, 77, 'W'); c.set(25, 77, 'W');
            }
            "seedling" => {
                // small sapling
                c.fill_ellipse(24, 62, 7, 6, 'M', 'm');
                c.fill_ellipse(20, 60, 4, 3, 'M', 'm');
                c.fill_ellipse(28, 60, 4, 3, 'M', 'm');
                // little trunk
                for y in 64..78 {
                    c.set(23, y, 'b');
                    c.set(24, y, 'b');
                }
                c.set(23, 78, 'd'); c.set(24, 78, 'd');
            }
            "harvested" => {
                // stump with rings
                c.fill_ellipse(24, 75, 9, 4, 'b', 'd');
                c.fill_ellipse(24, 74, 6, 3, 'D', 'b');
                c.fill_ellipse(24, 74, 3, 2, 'd', 'D');
                c.set(24, 74, 'D');
                // a couple of scattered chips on ground
                c.set(14, 78, 'b'); c.set(34, 78, 'b');
                c.set(12, 79, 'd'); c.set(36, 79, 'd');
            }
            "mature" => {
                // big lobed canopy — multiple overlapping ellipses
                c.fill_ellipse(24, 30, 22, 28, 'G', 'm');
                c.fill_ellipse(14, 23, 11, 10, 'M', 'm');
                c.fill_ellipse(34, 23, 11, 10, 'M', 'm');
                c.fill_ellipse(24, 13, 11, 9, 'M', 'm');
                c.fill_ellipse(24, 38, 14, 12, 'm', 'G');
                // dappled highlights
                c.fill_ellipse(17, 27, 4, 3, 'L', 'M');
                c.fill_ellipse(31, 25, 4, 3, 'L', 'M');
                c.fill_ellipse(24, 18, 4, 3, 'L', 'M');
                c.fill_ellipse(20, 38, 3, 2, 'L', 'M');
                c.fill_ellipse(29, 36, 3, 2, 'L', 'M');
                // trunk (4 px wide, with shading)
                for y in 48..78 {
                    c.set(22, y, 'd');
                    c.set(23, y, 'D');
                    c.set(24, y, 'D');
                    c.set(25, y, 'b');
                }
                // bark ridges
                for y in (50..76).step_by(6) {
                    c.set(22, y, 'D');
                    c.set(25, y, 'd');
                }
                // root flare
                c.set(20, 78, 'D'); c.set(21, 78, 'D');
                c.set(26, 78, 'D'); c.set(27, 78, 'D');
                c.set(19, 79, 'D'); c.set(22, 79, 'D');
                c.set(25, 79, 'D'); c.set(28, 79, 'D');
            }
            "overripe" => {
                // autumn colours
                c.fill_ellipse(24, 30, 22, 28, 'y', 'o');
                c.fill_ellipse(14, 23, 11, 10, 'o', 'R');
                c.fill_ellipse(34, 23, 11, 10, 'o', 'R');
                c.fill_ellipse(24, 13, 11, 9, 'r', 'R');
                c.fill_ellipse(24, 38, 14, 12, 'o', 'y');
                // scattered fallen leaves around base
                c.set(14, 78, 'o'); c.set(34, 78, 'o');
                c.set(12, 79, 'r'); c.set(36, 79, 'r');
                c.set(18, 79, 'y'); c.set(30, 79, 'y');
                // trunk
                for y in 48..78 {
                    c.set(22, y, 'd');
                    c.set(23, y, 'D');
                    c.set(24, y, 'D');
                    c.set(25, y, 'b');
                }
                c.set(20, 78, 'D'); c.set(21, 78, 'D');
                c.set(26, 78, 'D'); c.set(27, 78, 'D');
            }
            _ => panic!("unknown stem {stem}"),
        }
        c.into_rows()
    }

    // ── plumbing ───────────────────────────────────────────────────────

    fn rows_for(form: PlantForm, stem: &str) -> Vec<String> {
        use PlantForm::*;
        match form {
            Grass => grass_template(stem),
            Forb => forb_template(stem),
            Shrub => shrub_template(stem),
            Vine => vine_template(stem),
            Tree => tree_template(stem),
            Aquatic => aquatic_template(stem),
            Cactus => cactus_template(stem),
            Tuber => tuber_template(stem),
        }
    }

    /// v0 = base; v1 = horizontal mirror; v2 = palette shifted one band lighter.
    /// Works on any canvas size since it only touches chars.
    fn variant_transform(rows: &[String], variant: u8) -> Vec<String> {
        match variant {
            0 => rows.to_vec(),
            1 => rows
                .iter()
                .map(|s| s.chars().rev().collect::<String>())
                .collect(),
            _ => rows
                .iter()
                .map(|s| {
                    s.chars()
                        .map(|c| match c {
                            'g' => 'G',
                            'G' => 'm',
                            'm' => 'M',
                            'M' => 'L',
                            'd' => 'D',
                            'D' => 'b',
                            'r' => 'R',
                            other => other,
                        })
                        .collect()
                })
                .collect(),
        }
    }

    fn write_template(out_dir: &PathBuf, stem: &str, rows: &[String]) {
        assert!(!rows.is_empty(), "empty rows for {stem}");
        let h = rows.len() as u32;
        let w = rows[0].chars().count() as u32;
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                r.chars().count(),
                w as usize,
                "row {i} of {stem} not {w} chars: {r:?}"
            );
        }
        let mut img: image::RgbaImage = image::ImageBuffer::new(w, h);
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                let c = lookup(ch);
                img.put_pixel(x as u32, y as u32, image::Rgba([c.r, c.g, c.b, c.a]));
            }
        }
        fs::create_dir_all(out_dir).unwrap();
        let path = out_dir.join(format!("{stem}.png"));
        img.save(&path).unwrap_or_else(|e| panic!("save {path:?}: {e}"));
    }

    fn write_variant(out_dir: &PathBuf, stem: &str, variant: u8, rows: &[String]) {
        let xformed = variant_transform(rows, variant);
        let filename = if matches!(stem, "seed" | "harvested") {
            stem.to_string()
        } else {
            format!("{stem}_{variant}")
        };
        write_template(out_dir, &filename, &xformed);
    }

    fn form_slug_for(form: PlantForm) -> &'static str {
        match form {
            PlantForm::Grass => "grass",
            PlantForm::Forb => "forb",
            PlantForm::Shrub => "shrub",
            PlantForm::Vine => "vine",
            PlantForm::Tree => "tree",
            PlantForm::Aquatic => "aquatic",
            PlantForm::Cactus => "cactus",
            PlantForm::Tuber => "tuber",
        }
    }

    #[test]
    #[ignore = "PNG generator — run with --ignored when regenerating form fallback art"]
    fn gen_form_fallback_pngs() {
        let forms = PlantForm::ALL;
        let stems: &[(&str, u8)] = &[
            ("seed", 1),
            ("seedling", 3),
            ("harvested", 1),
            ("mature", 3),
            ("overripe", 3),
        ];
        for form in forms {
            let folder = PathBuf::from(format!(
                "assets/textures/plants/_form_{}",
                form_slug_for(form)
            ));
            for (stem, vcount) in stems {
                let base = rows_for(form, stem);
                for v in 0..*vcount {
                    write_variant(&folder, stem, v, &base);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sprite audit — non-ignored regression coverage (run in `cargo test`).
//
// Guards against the "invisible plant" class of bug: every plant must resolve
// to a visible sprite at every growth stage. The form-fallback layer is the
// anti-invisibility net (species folders ship no `seed.png`, so the Seed stage
// of every plant routes to `_form_<form>/seed.png`), and these tests fail if
// that net has a hole or a sprite is too faint to read against terrain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod audit {
    use super::*;

    /// `(stem, max_variants)` for the early stages we hold to a visibility
    /// floor. Mature/Overripe are large by construction and not policed here.
    /// Floors: `(min non-transparent px, min painted-bbox edge)`.
    const FLOORS: &[(&str, u8, u32, u32)] = &[
        ("seed", 1, 16, 4),
        ("seedling", 3, 20, 1),
        ("harvested", 1, 16, 4),
    ];

    fn form_dir(form: PlantForm) -> String {
        format!("{ASSET_ROOT_FS}/_form_{}", form_slug(form))
    }

    /// Decode a shipped PNG → (non-transparent pixel count, bbox width, height).
    fn opaque_stats(path: &str) -> (u32, u32, u32) {
        let img = image::open(path)
            .unwrap_or_else(|e| panic!("audit: cannot open {path}: {e}"))
            .to_rgba8();
        let (mut count, mut min_x, mut min_y, mut max_x, mut max_y) =
            (0u32, u32::MAX, u32::MAX, 0u32, 0u32);
        for (x, y, px) in img.enumerate_pixels() {
            if px.0[3] != 0 {
                count += 1;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        if count == 0 {
            return (0, 0, 0);
        }
        (count, max_x - min_x + 1, max_y - min_y + 1)
    }

    /// The real anti-invisibility invariant: every form ships every stage +
    /// variant, so the resolver's Tier-2 fallback always covers any plant and
    /// legacy ASCII is never reached.
    #[test]
    fn form_fallback_complete() {
        for form in PlantForm::ALL {
            let dir = form_dir(form);
            for (_stage, stem, max_variants) in STAGE_LAYOUT {
                for v in 0..max_variants {
                    let path = format!("{dir}/{}", file_name(stem, max_variants, v));
                    assert!(
                        std::path::Path::new(&path).exists(),
                        "missing form-fallback sprite {path} — regenerate with \
                         `cargo test --bin civgame -- --ignored gen_form_fallback_pngs`",
                    );
                }
            }
        }
    }

    /// Every catalog species resolves to a sprite folder on disk. A species
    /// whose folder vanished/renamed would drop to form fallback silently;
    /// this surfaces it.
    #[test]
    fn species_folders_exist() {
        let catalog = crate::simulation::plant_catalog::catalog();
        for def in catalog.iter() {
            let dir = format!("{ASSET_ROOT_FS}/{}", def.sprite_folder());
            assert!(
                std::path::Path::new(&dir).is_dir(),
                "species {:?} has no sprite folder at {dir}",
                def.sprite_folder(),
            );
        }
    }

    /// Early-stage form sprites must clear a visibility floor. Reads the
    /// *shipped* PNG (not the template) so a template edit that wasn't
    /// regenerated also fails here.
    #[test]
    fn form_fallback_visibility_floor() {
        for form in PlantForm::ALL {
            let dir = form_dir(form);
            for &(stem, max_variants, min_px, min_bbox) in FLOORS {
                for v in 0..max_variants {
                    let path = format!("{dir}/{}", file_name(stem, max_variants, v));
                    let (count, w, h) = opaque_stats(&path);
                    assert!(
                        count >= min_px,
                        "{path}: only {count} non-transparent px (floor {min_px}) \
                         — sprite reads as invisible",
                    );
                    assert!(
                        w >= min_bbox && h >= min_bbox,
                        "{path}: painted bbox {w}x{h} below {min_bbox}x{min_bbox} floor",
                    );
                }
            }
        }
    }
}
