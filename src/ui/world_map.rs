use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::world::globe::{Globe, GLOBE_HEIGHT, GLOBE_WIDTH};

#[derive(Resource, Default)]
pub struct WorldMapOpen(pub bool);

/// Cache so we don't re-upload the texture every frame unless globe changed.
#[derive(Resource, Default)]
pub struct WorldMapTexture {
    handle: Option<egui::TextureHandle>,
    /// Last count of explored cells when we built the texture — rebuild when it changes.
    last_explored: u32,
}

pub fn world_map_toggle_system(keys: Res<ButtonInput<KeyCode>>, mut open: ResMut<WorldMapOpen>) {
    if keys.just_pressed(KeyCode::Tab) {
        open.0 = !open.0;
    }
}

pub fn world_map_system(
    mut contexts: EguiContexts,
    open: Res<WorldMapOpen>,
    globe: Res<Globe>,
    mut tex_cache: ResMut<WorldMapTexture>,
    camera_q: Query<&Transform, With<Camera>>,
    mut camera_state: ResMut<crate::rendering::camera::CameraState>,
) {
    // Suppress camera input while map is open
    if open.0 {
        camera_state.drag_origin = None;
    }

    if !open.0 {
        return;
    }

    let ctx = contexts.ctx_mut();

    // Rebuild texture if explored cells changed
    let explored_count = globe.cells.iter().filter(|c| c.explored).count() as u32;
    if tex_cache.handle.is_none() || tex_cache.last_explored != explored_count {
        let pixels = build_globe_image(&globe);
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [GLOBE_WIDTH as usize, GLOBE_HEIGHT as usize],
            &pixels,
        );
        let handle = ctx.load_texture("world_map", image, egui::TextureOptions::NEAREST);
        tex_cache.handle = Some(handle);
        tex_cache.last_explored = explored_count;
    }

    let Some(ref tex) = tex_cache.handle else {
        return;
    };

    egui::Window::new("World Map")
        .title_bar(true)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label("Tab — close    |    Biomes: 🟦 Ocean  ⬜ Tundra  🟫 Taiga  🟩 Temperate  🌿 Grassland  💚 Tropical  🟨 Desert  ⬛ Mountain");

            // Scale image to fit within ~800×400 px
            let scale = 10.0f32;
            let img_w = GLOBE_WIDTH  as f32 * scale;
            let img_h = GLOBE_HEIGHT as f32 * scale;

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(img_w, img_h),
                egui::Sense::hover(),
            );

            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            // Draw current camera position as a white rectangle
            if let Ok(cam_t) = camera_q.get_single() {
                use crate::world::chunk::CHUNK_SIZE;
                use crate::world::globe::GLOBE_CELL_CHUNKS;
                use crate::world::terrain::TILE_SIZE;

                let tile_x = (cam_t.translation.x / TILE_SIZE) as f32;
                let tile_y = (cam_t.translation.y / TILE_SIZE) as f32;
                let total_tiles_x = (GLOBE_WIDTH  * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;
                let total_tiles_y = (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;

                let nx = tile_x / total_tiles_x;
                let ny = 1.0 - tile_y / total_tiles_y; // flip Y for screen coords

                // Visible window ≈ LOAD_RADIUS*2 chunks, shown as a small rect
                use crate::world::chunk_streaming::LOAD_RADIUS;
                let view_w = (LOAD_RADIUS * 2) as f32 / (GLOBE_WIDTH  * GLOBE_CELL_CHUNKS) as f32;
                let view_h = (LOAD_RADIUS * 2) as f32 / (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS) as f32;

                let cx = rect.min.x + nx * img_w;
                let cy = rect.min.y + ny * img_h;
                let view_rect = egui::Rect::from_center_size(
                    egui::pos2(cx, cy),
                    egui::vec2(view_w * img_w, view_h * img_h),
                );
                ui.painter().rect_stroke(
                    view_rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::WHITE),
                );
            }
        });
}

fn build_globe_image(globe: &Globe) -> Vec<u8> {
    let mut pixels = vec![0u8; (GLOBE_WIDTH * GLOBE_HEIGHT * 4) as usize];

    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let idx = ((gy * GLOBE_WIDTH + gx) * 4) as usize;
            let cell = globe.cell(gx, gy).unwrap();

            let rgba = if !cell.explored {
                [25, 25, 25, 255] // unexplored — dark gray
            } else {
                let mut c = cell.biome.color();
                // Tint faction cells slightly
                if cell.faction_id != 0 {
                    c[0] = c[0].saturating_add(30);
                    c[2] = c[2].saturating_sub(20);
                }
                c
            };

            pixels[idx] = rgba[0];
            pixels[idx + 1] = rgba[1];
            pixels[idx + 2] = rgba[2];
            pixels[idx + 3] = rgba[3];
        }
    }

    // Flip Y so north is up (Bevy Y=0 is bottom)
    let row_bytes = (GLOBE_WIDTH * 4) as usize;
    let mut flipped = vec![0u8; pixels.len()];
    for row in 0..GLOBE_HEIGHT as usize {
        let src = row * row_bytes;
        let dst = (GLOBE_HEIGHT as usize - 1 - row) * row_bytes;
        flipped[dst..dst + row_bytes].copy_from_slice(&pixels[src..src + row_bytes]);
    }
    flipped
}
