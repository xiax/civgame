//! Full-screen world-map UI for picking the player's starting mega-chunk.
//! Runs only in `GameState::SpawnSelect`. Click on a cell → write
//! `PendingSpawn` and transition to `GameState::Playing`.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::game_state::{GameState, PendingSpawn};
use crate::simulation::region::MegaChunkCoord;
use crate::ui::world_map::build_globe_image;
use crate::world::globe::{
    Biome, Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS,
};

#[derive(Resource, Default)]
pub struct SpawnSelectTexture {
    handle: Option<egui::TextureHandle>,
}

pub fn spawn_select_system(
    mut contexts: EguiContexts,
    globe: Res<Globe>,
    mut tex_cache: ResMut<SpawnSelectTexture>,
    mut pending: ResMut<PendingSpawn>,
    mut next_state: ResMut<NextState<GameState>>,
) {
    let ctx = contexts.ctx_mut();

    if tex_cache.handle.is_none() {
        let pixels = build_globe_image(&globe, false);
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [GLOBE_WIDTH as usize, GLOBE_HEIGHT as usize],
            &pixels,
        );
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
