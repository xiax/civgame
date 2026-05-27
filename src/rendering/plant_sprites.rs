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

use ahash::AHashMap;
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
// Templates are hand-drawn against `pixel_art::WARM_PALETTE`.
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

    fn write_template(out_dir: &PathBuf, stem: &str, rows: &[&str]) {
        let h = rows.len() as u32;
        let w = rows[0].chars().count() as u32;
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

    /// Hand-drawn 16×16 (non-tree) or 32×32 (tree) per (form, stage, variant).
    /// Stages: Seed (single), Seedling (3v), Harvested (single), Mature (3v),
    /// Overripe (3v). 11 PNGs per form × 8 forms = 88 PNGs.
    fn rows_for(form: PlantForm, stem: &str, variant: u8) -> Vec<&'static str> {
        use PlantForm::*;
        match form {
            Grass => grass_template(stem, variant),
            Forb => forb_template(stem, variant),
            Shrub => shrub_template(stem, variant),
            Vine => vine_template(stem, variant),
            Tree => tree_template(stem, variant),
            Aquatic => aquatic_template(stem, variant),
            Cactus => cactus_template(stem, variant),
            Tuber => tuber_template(stem, variant),
        }
    }

    // ── 16×16 templates ────────────────────────────────────────────────

    fn grass_template(stem: &str, v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
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
                "................",
                "......WW........",
                ".....WBBW.......",
                "......WW........",
            ]),
            "seedling" => to_vec(&[
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "................",
                "......m.........",
                "......M.........",
                ".....LMl........",
                "......M.........",
                "......m.........",
                "......m.........",
                "................",
            ]),
            "harvested" => to_vec(&[
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
                "......B.B.......",
                "......B.B.......",
                "......B.B.......",
                ".....BBBBB......",
                "................",
            ]),
            "mature" => {
                let _ = v;
                to_vec(&[
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
                ])
            }
            "overripe" => to_vec(&[
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

    fn forb_template(stem: &str, _v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......WW........", ".....WBBW.......",
                "......WW........",
            ]),
            "seedling" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "......G.........", ".....MGM........", "......G.........",
                "......G.........", "......G.........", ".....BGB........",
                "................",
            ]),
            "harvested" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "......b.........", ".....BbB........", "......b.........",
                "................",
            ]),
            "mature" => to_vec(&[
                "................", "................", "................",
                "................", "......r.........", ".....rRr........",
                "......o.........", "....G.M.G.......", ".....GMG........",
                "......M.........", ".....MGM........", "......G.........",
                "......G.........", ".....GgG........", "......g.........",
                "................",
            ]),
            "overripe" => to_vec(&[
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

    fn shrub_template(stem: &str, _v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......WW........", ".....WBBW.......",
                "......WW........",
            ]),
            "seedling" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                ".....mMm........", "....MGGGM.......", ".....mGm........",
                "......G.........", "......G.........", "......b.........",
                "................",
            ]),
            "harvested" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "....G.G.G.......",
                "....GGGGG.......", ".....GGG........", "......G.........",
                "......G.........", "......G.........", "......b.........",
                "................",
            ]),
            "mature" => to_vec(&[
                "................", "................", "................",
                "................", "....GMG.GMG.....", "...MGGMGMGGM....",
                "..MGGGGMGGGGM...", "..MGGMGGGMGGM...", "..GGGGMGMGGGG...",
                "...MGGGMGGGM....", "....MGGGMGM.....", ".....MGGG.......",
                "......b.........", "......b.........", "......b.........",
                "................",
            ]),
            "overripe" => to_vec(&[
                "................", "................", "................",
                "................", "....bGb.bGb.....", "...GGgbGGbgGG...",
                "..GGgggGggggGG..", "..GgGgrGgRgGgG..", "..GggGGGrGGggG..",
                "...GGGgGGGGG....", "....GGGgGgG.....", ".....GggG.......",
                "......b.........", "......b.........", "......b.........",
                "................",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn vine_template(stem: &str, _v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......WW........", ".....WBBW.......",
                "......WW........",
            ]),
            "seedling" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "....m...........", "...MGm..........",
                "....G...........", "....G...........", "....G...........",
                "................",
            ]),
            "harvested" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "...G.G.G.G......",
                "....GG.GG.......", "...G.GGG.G......", "....G...G.......",
                "...G.....G......", "...G.....G......", "...b.....b......",
                "................",
            ]),
            "mature" => to_vec(&[
                "................", "................", "....M...M.......",
                "...MGM.MGM......", "....MG.GM.......", "...G.MGM.G......",
                "..MG.MGM.GM.....", ".G..MGGGM..G....", "..G.MGGGM.G.....",
                "...M.GGG.M......", "..MG.GMG.GM.....", "...M.MGM.M......",
                "....MGGGM.......", ".....MGM........", "......b.........",
                "................",
            ]),
            "overripe" => to_vec(&[
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

    fn aquatic_template(stem: &str, _v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                ".....iiHii......", "....iIHHHIi.....", "....iiHHHii.....",
                ".....iiiii......",
            ]),
            "seedling" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "......m.........", "......G.........", "......G.........",
                ".....iGi........", "....iIGIi.......", "....iiGii.......",
                ".....iiiii......",
            ]),
            "harvested" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......b.........", "......b.........",
                ".....ibi........", "....iIbIi.......", "....iibii.......",
                ".....iiiii......",
            ]),
            "mature" => to_vec(&[
                "................", "......M.........", "....MMGMM.......",
                "...MGGMGGM......", "....MGGGM.......", "......G.........",
                "......G.........", "......G.........", "......G.........",
                "......G.........", "......G.........", ".....iGi........",
                "....iIGIi.......", "...iiIGIii......", "...iiHIHii......",
                "....iiiii.......",
            ]),
            "overripe" => to_vec(&[
                "................", "......b.........", "....bbGbb.......",
                "...bGGbGGb......", "....bGGGb.......", "......G.........",
                "......G.........", "......b.........", "......b.........",
                "......b.........", "......b.........", ".....ibi........",
                "....iIbIi.......", "...iiIbIii......", "...iiHIHii......",
                "....iiiii.......",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn cactus_template(stem: &str, _v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......EE........", ".....EBBE.......",
                "......EE........",
            ]),
            "seedling" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......G.........", "......G.........",
                "......G.........", ".....GGG........", ".....EEE........",
                "................",
            ]),
            "harvested" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "......b.........", "......b.........", "......b.........",
                ".....bbb........", ".....bbb........", ".....EEE........",
                "................",
            ]),
            "mature" => to_vec(&[
                "................", "......G.........", "......G.........",
                "......G.........", "...G..G..G......", "...G..G..G......",
                "...G.GGG.G......", "...G.GGG.G......", "...GGGGGGG......",
                "...gGGGGGg......", "...gGGGGGg......", "....GGGGG.......",
                "....GGGGG.......", ".....GGG........", ".....EEE........",
                "................",
            ]),
            "overripe" => to_vec(&[
                "................", "......b.........", "......b.........",
                "......b.........", "...b..b..b......", "...b..b..b......",
                "...b.bGb.b......", "...b.bGb.b......", "...bbGGGbb......",
                "...DbGGGbD......", "...DbGGGbD......", "....bGGGb.......",
                "....bGGGb.......", ".....bbb........", ".....EEE........",
                "................",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn tuber_template(stem: &str, _v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", ".....SBBS.......",
                "......SS........",
            ]),
            "seedling" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "......G.........", ".....MGM........",
                ".....GMG........", "................", ".....SBS........",
                "......SS........",
            ]),
            "harvested" => to_vec(&[
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", "................",
                "................", "................", ".....sss........",
                "......sS........",
            ]),
            "mature" => to_vec(&[
                "................", "................", "................",
                "................", ".....MGM........", "....MGMGM.......",
                "...MGGGMGM......", "....MGMGM.......", ".....MGM........",
                "......G.........", "......G.........", ".....bGb........",
                "....bBBBb.......", "...bBBeBBb......", "....bBBBb.......",
                ".....bbb........",
            ]),
            "overripe" => to_vec(&[
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

    // ── 32×32 tree templates ──────────────────────────────────────────

    fn tree_template(stem: &str, v: u8) -> Vec<&'static str> {
        match stem {
            "seed" => to_vec_32(&[
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "..............WW................",
                ".............WBBW...............",
                "..............WW................",
                "................................",
                "................................",
            ]),
            "seedling" => to_vec_32(&[
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "..............M.................",
                ".............MGM................",
                "............MGGGM...............",
                ".............MGM................",
                "..............G.................",
                "..............G.................",
                "..............G.................",
                "..............b.................",
                "..............b.................",
                ".............bbb................",
                "............bbsbb...............",
                "................................",
                "................................",
            ]),
            "harvested" => to_vec_32(&[
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "................................",
                "............d...................",
                "...........dDd..................",
                "..........dDDDd.................",
                "...........DDD..................",
                "............D...................",
                "............D...................",
                "............D...................",
                "............D...................",
                "............D...................",
                "............D...................",
                "...........DDD..................",
                "...........DDD..................",
                "..........DDDDD.................",
                ".........DDDDDDD................",
                "................................",
                "................................",
            ]),
            "mature" => match v {
                0 => to_vec_32(&[
                    "................................",
                    "................................",
                    "................................",
                    "............GGG.................",
                    "..........GGmGmGG...............",
                    ".........GmGmmmmGG..............",
                    "........GGmmGmmGmGG.............",
                    ".......GGmGmMmMGGmGG............",
                    "......GGmmGMmMmMmGGGG...........",
                    ".....GGmGmMMmMmMGmmGG...........",
                    ".....GGmMmMmMmMmMmGGG...........",
                    "....GGmmGMmMmMmMmMmGG...........",
                    "....GGGmMmMmMmMGmMmGG...........",
                    "....GGmGmMGmMmMmMmGG............",
                    ".....GGmMmMmMmGmMmGG............",
                    ".....GGmmGMmMmMmMGGG............",
                    "......GGmGmMmMmMmGG.............",
                    ".......GGmMGmMmGGGG.............",
                    "........GGGGmMGGGG..............",
                    "..........GGmGGG................",
                    ".............b..................",
                    "............dDb.................",
                    "............dDb.................",
                    "............dDb.................",
                    "...........dDDDb................",
                    "...........dDDDb................",
                    "..........dDDDDDb...............",
                    "..........dDDDDDb...............",
                    ".........dDDDDDDDb..............",
                    "........dDDDDDDDDDb.............",
                    "................................",
                    "................................",
                ]),
                1 => to_vec_32(&[
                    "................................",
                    "................................",
                    ".............GG.................",
                    "...........GGmGGG...............",
                    "..........GmGmmGGG..............",
                    ".........GGmGmmGmGG.............",
                    "........GmGmMmMmmGGG............",
                    ".......GGmGmMmMGmmGG............",
                    "......GGGmMmMmMmMmGGGG..........",
                    ".....GGmGmMGMmMmMGmGG...........",
                    "....GGmGmMmMmMmMmMmGG...........",
                    "....GGmmGMmMmMmMmMGGG...........",
                    "....GGGmMmMGMmMmMmGG............",
                    "....GGmGmMmMmMGMmGG.............",
                    ".....GGmMmMmMmMmGGG.............",
                    ".....GGGmGMmMmGmGG..............",
                    "......GGmGmMmMmGGG..............",
                    ".......GGmGMmMmGG...............",
                    "........GGGGmMGGG...............",
                    "..........GGGGG.................",
                    ".............b..................",
                    "............bDb.................",
                    "............dDb.................",
                    "............dDb.................",
                    "...........bDDDb................",
                    "...........dDDDb................",
                    "..........bDDDDDb...............",
                    "..........dDDDDDb...............",
                    ".........dDDDDDDDb..............",
                    "........bDDDDDDDDDb.............",
                    "................................",
                    "................................",
                ]),
                _ => to_vec_32(&[
                    "................................",
                    "................................",
                    "...........GGGG.................",
                    "..........GmGmGGG...............",
                    ".........GGmmGmGGG..............",
                    "........GGmmmGmGmGG.............",
                    ".......GGmGmMmMmmGGG............",
                    "......GGmGmMmMmMGmGGG...........",
                    "......GGmMmMmMmMmMGGG...........",
                    ".....GGGmMmMGMmMmMmGG...........",
                    ".....GGmMmMmMmMmMmGGG...........",
                    "....GGGmMmMmMmMmMmGGG...........",
                    "....GGmGmMmMmMmMmGG.............",
                    "....GGmmGMmMmMmGGG..............",
                    "....GGGmMmMmMmGGG...............",
                    ".....GGmMmMGGGGG................",
                    ".....GGGmMmMGG..................",
                    "......GGGmGGG...................",
                    ".......GGGGG....................",
                    "........GGG.....................",
                    ".............b..................",
                    "............dDb.................",
                    "............dDb.................",
                    "............dDb.................",
                    "...........dDDDb................",
                    "...........dDDDb................",
                    "..........dDDDDDb...............",
                    "..........dDDDDDb...............",
                    ".........dDDDDDDDb..............",
                    "........dDDDDDDDDDb.............",
                    "................................",
                    "................................",
                ]),
            },
            "overripe" => to_vec_32(&[
                "................................",
                "................................",
                "............oyo.................",
                "..........oyoRoyo...............",
                ".........oRoRyyoRo..............",
                "........oRoyoRyooRo.............",
                ".......oRoyoyoyooyRo............",
                "......oRoyoRyoRoyoRGo...........",
                "......oRyoyoRyoyoyRGo...........",
                ".....oRoyoRGRoRoyoyRG...........",
                ".....oRyoyoRyoRyooyRGG..........",
                "....oRoyoRyoyoRyoyoRGG..........",
                "....oRoyoyoyoyoyoyRGG...........",
                "....oRyoRyoRyoyoRGG.............",
                ".....oRoyoyoyRGGG...............",
                ".....oRyoRyGGGG.................",
                "......oRGGGGG...................",
                ".......GGGGG....................",
                "........GGG.....................",
                ".........G......................",
                ".............b..................",
                "............dDb.................",
                "............dDb.................",
                "............dDb.................",
                "...........dDDDb................",
                "...........dDDDb................",
                "..........dDDDDDb...............",
                "..........dDDDDDb...............",
                ".........dDDDDDDDb..............",
                "........dDDDDDDDDDb.............",
                "................................",
                "................................",
            ]),
            _ => panic!("unknown stem {stem}"),
        }
    }

    fn to_vec(rows: &[&'static str]) -> Vec<&'static str> {
        assert_eq!(rows.len(), 16, "non-tree template must be 16 rows");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.chars().count(), 16, "row {i} not 16 chars: {r:?}");
        }
        rows.to_vec()
    }

    fn to_vec_32(rows: &[&'static str]) -> Vec<&'static str> {
        assert_eq!(rows.len(), 32, "tree template must be 32 rows");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.chars().count(), 32, "row {i} not 32 chars: {r:?}");
        }
        rows.to_vec()
    }

    /// Apply per-variant transforms so variants 1/2 differ from variant 0
    /// without hand-authoring three distinct templates per stage. v0 is
    /// the base; v1 flips horizontally; v2 shifts foliage one band lighter
    /// (G→m, m→M, M→L) and trunk one band lighter (d→D, D→b).
    fn variant_transform(rows: &[&'static str], variant: u8) -> Vec<String> {
        match variant {
            0 => rows.iter().map(|s| s.to_string()).collect(),
            1 => rows.iter().map(|s| s.chars().rev().collect()).collect(),
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

    fn write_variant(out_dir: &PathBuf, stem: &str, variant: u8, rows: &[&'static str]) {
        let xformed: Vec<String> = variant_transform(rows, variant);
        let refs: Vec<&str> = xformed.iter().map(|s| s.as_str()).collect();
        let filename = if matches!(stem, "seed" | "harvested") {
            stem.to_string()
        } else {
            format!("{stem}_{variant}")
        };
        write_template(out_dir, &filename, &refs);
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
        let forms = [
            PlantForm::Grass,
            PlantForm::Forb,
            PlantForm::Shrub,
            PlantForm::Vine,
            PlantForm::Tree,
            PlantForm::Aquatic,
            PlantForm::Cactus,
            PlantForm::Tuber,
        ];
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
                let base_rows = rows_for(form, stem, 0);
                let base_refs: Vec<&'static str> = base_rows.clone();
                for v in 0..*vcount {
                    write_variant(&folder, stem, v, &base_refs);
                }
            }
        }
    }
}
