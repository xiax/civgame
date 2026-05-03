use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::region::{MegaChunkCoord, SettledRegions};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS};

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
    mut settled: ResMut<SettledRegions>,
    mut camera_q: Query<&mut Transform, With<Camera>>,
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
        let pixels = build_globe_image(&globe, true);
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
            ui.label("Tab — close   |   Click a settled region to switch view");

            // Scale image to fit within ~800×400 px
            let scale = 10.0f32;
            let img_w = GLOBE_WIDTH  as f32 * scale;
            let img_h = GLOBE_HEIGHT as f32 * scale;

            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(img_w, img_h),
                egui::Sense::click(),
            );

            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            use crate::world::chunk::CHUNK_SIZE;
            use crate::world::terrain::TILE_SIZE;

            let total_tiles_x = (GLOBE_WIDTH  * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;
            let total_tiles_y = (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;

            // Mega-chunk overlay: outline every settled mega-chunk in faction colour.
            let mc_grid_w = (GLOBE_WIDTH  * GLOBE_CELL_CHUNKS) / MEGACHUNK_SIZE_CHUNKS;
            let mc_grid_h = (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS) / MEGACHUNK_SIZE_CHUNKS;
            let cell_w = img_w / mc_grid_w as f32;
            let cell_h = img_h / mc_grid_h as f32;

            for region in settled.by_id.values() {
                let (mx, my) = region.megachunk;
                if mx < 0 || my < 0 || mx >= mc_grid_w || my >= mc_grid_h {
                    continue;
                }
                // Y is flipped (north up).
                let screen_my = mc_grid_h - 1 - my;
                let r = egui::Rect::from_min_size(
                    egui::pos2(
                        rect.min.x + mx as f32 * cell_w,
                        rect.min.y + screen_my as f32 * cell_h,
                    ),
                    egui::vec2(cell_w, cell_h),
                );
                let color = if region.player_owned {
                    egui::Color32::from_rgba_premultiplied(255, 220, 100, 200)
                } else {
                    egui::Color32::from_rgba_premultiplied(200, 100, 100, 180)
                };
                ui.painter().rect_stroke(r, 0.0, egui::Stroke::new(2.0, color));
            }

            // Draw current camera position as a white rectangle.
            if let Ok(cam_t) = camera_q.get_single() {
                let tile_x = cam_t.translation.x / TILE_SIZE;
                let tile_y = cam_t.translation.y / TILE_SIZE;

                let nx = tile_x / total_tiles_x;
                let ny = 1.0 - tile_y / total_tiles_y;

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

            // Click handling: jump camera to the settled region under cursor.
            if let Some(pos) = response.interact_pointer_pos() {
                if response.clicked() && rect.contains(pos) {
                    let mx = ((pos.x - rect.min.x) / cell_w) as i32;
                    let screen_my = ((pos.y - rect.min.y) / cell_h) as i32;
                    let my = mc_grid_h - 1 - screen_my;
                    let target = (mx.clamp(0, mc_grid_w - 1), my.clamp(0, mc_grid_h - 1));

                    // Only respond if a region is actually settled there.
                    if let Some(&region_id) = settled.by_megachunk.get(&target) {
                        // Bookmark current camera pos under the region currently containing it.
                        if let Ok(mut cam_t) = camera_q.get_single_mut() {
                            let cur_tx = (cam_t.translation.x / TILE_SIZE).floor() as i32;
                            let cur_ty = (cam_t.translation.y / TILE_SIZE).floor() as i32;
                            let cur_mc = MegaChunkCoord::from_tile(cur_tx, cur_ty);
                            if let Some(&cur_region_id) = settled.by_megachunk.get(&cur_mc) {
                                if let Some(cr) = settled.by_id.get_mut(&cur_region_id) {
                                    cr.camera_bookmark = cam_t.translation.truncate();
                                }
                            }
                            // Jump to the target region's bookmark.
                            if let Some(target_region) = settled.by_id.get(&region_id) {
                                cam_t.translation.x = target_region.camera_bookmark.x;
                                cam_t.translation.y = target_region.camera_bookmark.y;
                            }
                        }
                    }
                }
            }
        });
}

/// Render the globe as an RGBA pixel buffer. When `respect_fog` is true,
/// unexplored cells render as dark grey (player-facing in-game map). When
/// false, every cell shows its true biome (used by spawn-select before any
/// exploration has happened).
pub fn build_globe_image(globe: &Globe, respect_fog: bool) -> Vec<u8> {
    let mut pixels = vec![0u8; (GLOBE_WIDTH * GLOBE_HEIGHT * 4) as usize];

    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            let idx = ((gy * GLOBE_WIDTH + gx) * 4) as usize;
            let cell = globe.cell(gx, gy).unwrap();

            let rgba = if respect_fog && !cell.explored {
                [25, 25, 25, 255] // unexplored — dark gray
            } else {
                let mut c = cell.biome.color();
                // Rivers tint blue, lakes deeper blue.
                if cell.is_river {
                    c[0] = (c[0] as u16 / 2 + 40) as u8;
                    c[1] = (c[1] as u16 / 2 + 80) as u8;
                    c[2] = c[2].saturating_add(80);
                }
                if cell.is_lake {
                    c[0] = 30;
                    c[1] = 80;
                    c[2] = 180;
                }
                // Tint faction cells slightly
                if respect_fog && cell.faction_id != 0 {
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
