//! Full-screen world-map UI for picking the player's starting mega-chunk.
//! Runs only in `GameState::SpawnSelect`. Click on a cell → write
//! `PendingSpawn` and transition to `GameState::Playing`.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::game_state::{
    EconomyPreset, GameStartOptions, GameState, PendingSpawn, RegenerateWorldRequest, WorldSeed,
};
use crate::simulation::faction::Lifestyle;
use crate::simulation::region::MegaChunkCoord;
use crate::simulation::technology::Era;
use crate::ui::world_map::{build_globe_image, WORLD_MAP_OVERSAMPLE};
use crate::world::globe::{
    Biome, Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS,
};

#[derive(Resource, Default)]
pub struct SpawnSelectTexture {
    handle: Option<egui::TextureHandle>,
    /// Buffered seed text so partial typing doesn't fight the resource.
    seed_text: String,
}

impl SpawnSelectTexture {
    /// Drop the cached egui texture handle so the next render rebuilds it
    /// from the current `Globe` (used after a reroll).
    pub fn clear_handle(&mut self) {
        self.handle = None;
    }
}

pub fn spawn_select_system(
    mut contexts: EguiContexts,
    globe: Res<Globe>,
    mut tex_cache: ResMut<SpawnSelectTexture>,
    mut pending: ResMut<PendingSpawn>,
    mut options: ResMut<GameStartOptions>,
    mut world_seed: ResMut<WorldSeed>,
    mut regen_w: EventWriter<RegenerateWorldRequest>,
    mut next_state: ResMut<NextState<GameState>>,
) {
    let ctx = contexts.ctx_mut();

    // Left side-panel: starting options (era, population, economy). The
    // values are committed when the player clicks a habitable mega-chunk.
    egui::SidePanel::left("spawn_options")
        .resizable(false)
        .default_width(240.0)
        .show(ctx, |ui| {
            ui.add_space(10.0);
            ui.heading("Starting options");
            ui.add_space(8.0);

            ui.label(egui::RichText::new("Era").strong());
            for era in [
                Era::Paleolithic,
                Era::Mesolithic,
                Era::Neolithic,
                Era::Chalcolithic,
                Era::BronzeAge,
            ] {
                ui.radio_value(&mut options.era, era, era.name());
            }
            ui.add_space(12.0);

            ui.label(egui::RichText::new("Player population").strong());
            ui.add(
                egui::Slider::new(&mut options.player_population, 5..=100)
                    .text("members"),
            );
            ui.add_space(12.0);

            ui.label(egui::RichText::new("Economy").strong());
            ui.radio_value(
                &mut options.economy,
                EconomyPreset::Subsistence,
                "Subsistence",
            );
            ui.label(
                egui::RichText::new("Chief allocates all labor; no private trade.")
                    .small()
                    .weak(),
            );
            ui.radio_value(&mut options.economy, EconomyPreset::Mixed, "Mixed");
            ui.label(
                egui::RichText::new(
                    "Households craft tools/cloth privately; staples stay communal.",
                )
                .small()
                .weak(),
            );
            ui.radio_value(&mut options.economy, EconomyPreset::Market, "Market");
            ui.label(
                egui::RichText::new(
                    "Every resource fully privatised; agents bid on chief postings.",
                )
                .small()
                .weak(),
            );
            ui.add_space(12.0);

            ui.label(egui::RichText::new("Lifestyle").strong());
            ui.radio_value(&mut options.lifestyle, Lifestyle::Settled, "Settled");
            ui.label(
                egui::RichText::new(
                    "Found a permanent settlement; build huts, walls, plots.",
                )
                .small()
                .weak(),
            );
            ui.radio_value(&mut options.lifestyle, Lifestyle::Nomadic, "Nomadic");
            ui.label(
                egui::RichText::new(
                    "No permanent home: tents, bedrolls, pack animals, seasonal migration.",
                )
                .small()
                .weak(),
            );
            ui.add_space(12.0);

            ui.label(egui::RichText::new("World").strong());
            // First-time init only — afterwards the buffer is owned by the
            // text field (typing) or explicitly overwritten by the Reroll
            // handler. Auto-mirroring would clobber mid-type input.
            if tex_cache.seed_text.is_empty() {
                tex_cache.seed_text = world_seed.0.to_string();
            }
            ui.horizontal(|ui| {
                ui.label("Seed:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut tex_cache.seed_text)
                        .desired_width(120.0),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(v) = tex_cache.seed_text.parse::<u64>() {
                        world_seed.0 = v;
                        regen_w.send(RegenerateWorldRequest);
                    }
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Apply seed").clicked() {
                    if let Ok(v) = tex_cache.seed_text.parse::<u64>() {
                        world_seed.0 = v;
                    }
                    regen_w.send(RegenerateWorldRequest);
                }
                if ui.button("Reroll").clicked() {
                    world_seed.0 = fastrand::u64(..);
                    tex_cache.seed_text = world_seed.0.to_string();
                    regen_w.send(RegenerateWorldRequest);
                }
            });
            ui.label(
                egui::RichText::new(
                    "Apply re-rolls climate, rivers, lakes, and per-tile terrain noise from the seed.",
                )
                .small()
                .weak(),
            );
        });

    if tex_cache.handle.is_none() {
        let (pixels, [w, h]) = build_globe_image(&globe, false, WORLD_MAP_OVERSAMPLE);
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels);
        tex_cache.handle = Some(ctx.load_texture(
            "spawn_select_globe",
            image,
            egui::TextureOptions::NEAREST,
        ));
    }
    let Some(ref tex) = tex_cache.handle else {
        return;
    };

    // Mega-chunk grid dimensions in climate-cells.
    let mc_cells_x = MEGACHUNK_SIZE_CHUNKS / GLOBE_CELL_CHUNKS; // 4
    let mc_cells_y = MEGACHUNK_SIZE_CHUNKS / GLOBE_CELL_CHUNKS; // 4
    let mc_grid_w = GLOBE_WIDTH / mc_cells_x; // 64
    let mc_grid_h = GLOBE_HEIGHT / mc_cells_y; // 32

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(20.0);
            ui.heading("Choose your starting region");
            ui.label("Click any habitable mega-chunk on the map to settle there.");
            ui.add_space(10.0);
        });

        let avail = ui.available_size();
        let img_aspect = GLOBE_WIDTH as f32 / GLOBE_HEIGHT as f32;
        let max_w = avail.x.min(1400.0);
        let max_h = (avail.y - 80.0).max(200.0);
        let (img_w, img_h) = if max_w / img_aspect <= max_h {
            (max_w, max_w / img_aspect)
        } else {
            (max_h * img_aspect, max_h)
        };

        ui.vertical_centered(|ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(img_w, img_h), egui::Sense::click());
            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            // Climate-cell sub-grid (drawn first so the bolder mega-chunk
            // grid sits on top). Drawn every 4 cells so the user sees a
            // finer subdivision without a wall-of-lines at full density.
            let sub_step = 4i32;
            let sub_nx = GLOBE_WIDTH / sub_step;
            let sub_ny = GLOBE_HEIGHT / sub_step;
            let sub_cw = img_w / sub_nx as f32;
            let sub_ch = img_h / sub_ny as f32;
            let sub_stroke =
                egui::Stroke::new(0.25, egui::Color32::from_rgba_premultiplied(0, 0, 0, 25));
            for i in 0..=sub_nx {
                let x = rect.min.x + i as f32 * sub_cw;
                ui.painter().line_segment(
                    [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                    sub_stroke,
                );
            }
            for j in 0..=sub_ny {
                let y = rect.min.y + j as f32 * sub_ch;
                ui.painter().line_segment(
                    [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                    sub_stroke,
                );
            }

            // Mega-chunk grid overlay.
            let cell_w = img_w / mc_grid_w as f32;
            let cell_h = img_h / mc_grid_h as f32;
            let stroke = egui::Stroke::new(0.5, egui::Color32::from_rgba_premultiplied(0, 0, 0, 60));
            for i in 0..=mc_grid_w {
                let x = rect.min.x + i as f32 * cell_w;
                ui.painter().line_segment(
                    [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                    stroke,
                );
            }
            for j in 0..=mc_grid_h {
                let y = rect.min.y + j as f32 * cell_h;
                ui.painter().line_segment(
                    [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                    stroke,
                );
            }

            // Hover info + click handler.
            if let Some(pos) = response.hover_pos() {
                if rect.contains(pos) {
                    let mx = ((pos.x - rect.min.x) / cell_w) as i32;
                    // Y is flipped (north up): convert pixel-Y back to grid-Y.
                    let my_screen = ((pos.y - rect.min.y) / cell_h) as i32;
                    let my = mc_grid_h - 1 - my_screen;
                    let mx = mx.clamp(0, mc_grid_w - 1);
                    let my = my.clamp(0, mc_grid_h - 1);

                    // Highlight the hovered cell.
                    let highlight_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            rect.min.x + mx as f32 * cell_w,
                            rect.min.y + my_screen as f32 * cell_h,
                        ),
                        egui::vec2(cell_w, cell_h),
                    );
                    let (tx, ty) = MegaChunkCoord::center_tile(mx, my);
                    let center_biome = sample_dominant_biome(&globe, mx, my);
                    let habitable = center_biome.is_habitable();
                    let stroke_color = if habitable {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_rgb(180, 80, 80)
                    };
                    ui.painter()
                        .rect_stroke(highlight_rect, 0.0, egui::Stroke::new(2.0, stroke_color));

                    // Tooltip with biome / coord info.
                    egui::show_tooltip(ctx, ui.layer_id(), egui::Id::new("spawn_tooltip"), |ui| {
                        ui.label(format!("Mega-chunk ({}, {})", mx, my));
                        ui.label(format!("Centre tile: ({}, {})", tx, ty));
                        ui.label(format!("Dominant biome: {}", center_biome.name()));
                        if !habitable {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 100, 100),
                                "Not habitable — pick a land region.",
                            );
                        } else {
                            ui.colored_label(
                                egui::Color32::from_rgb(140, 220, 140),
                                "Click to settle here.",
                            );
                        }
                    });

                    if response.clicked() && habitable {
                        pending.0 = Some((mx, my));
                        info!("Spawn picked: mega-chunk ({mx},{my}) tile ({tx},{ty})");
                        next_state.set(GameState::Playing);
                    }
                }
            }
        });

        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.label("🟦 Ocean   ⬜ Tundra   🟫 Taiga   🟩 Temperate   🌿 Grassland   💚 Tropical   🟨 Desert   ⬛ Mountain   💧 River   💦 Lake");
        });
    });
}

/// Find the most common biome among the climate cells covered by a mega-chunk.
fn sample_dominant_biome(globe: &Globe, mx: i32, my: i32) -> Biome {
    let cell_w = MEGACHUNK_SIZE_CHUNKS / GLOBE_CELL_CHUNKS;
    let gx0 = mx * cell_w;
    let gy0 = my * cell_w;
    let mut counts = [0u32; 8];
    for dy in 0..cell_w {
        for dx in 0..cell_w {
            let gx = (gx0 + dx).rem_euclid(GLOBE_WIDTH);
            let gy = (gy0 + dy).clamp(0, GLOBE_HEIGHT - 1);
            if let Some(c) = globe.cell(gx, gy) {
                counts[c.biome as usize] += 1;
            }
        }
    }
    let mut best = 0;
    let mut best_count = 0;
    for (i, &c) in counts.iter().enumerate() {
        if c > best_count {
            best_count = c;
            best = i;
        }
    }
    match best {
        0 => Biome::Ocean,
        1 => Biome::Tundra,
        2 => Biome::Taiga,
        3 => Biome::Temperate,
        4 => Biome::Grassland,
        5 => Biome::Tropical,
        6 => Biome::Desert,
        _ => Biome::Mountain,
    }
}
