use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::region::{MegaChunkCoord, SettledRegions};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS};

/// Pixels per climate cell when rendering the world map. 2× = sub-cell
/// bilinear biome detail that matches `biome::classify_at_tile`. Bumping
/// further is purely cosmetic (cell density is the data resolution).
pub const WORLD_MAP_OVERSAMPLE: u32 = 2;

#[derive(Resource, Default)]
pub struct WorldMapOpen(pub bool);

/// Cache so we don't re-upload the texture every frame unless globe changed.
#[derive(Resource, Default)]
pub struct WorldMapTexture {
    handle: Option<egui::TextureHandle>,
    /// Last count of explored cells when we built the texture — rebuild when it changes.
    last_explored: u32,
}

impl WorldMapTexture {
    /// Drop the cached egui texture handle so the next render rebuilds it
    /// from the current `Globe`.
    pub fn clear_handle(&mut self) {
        self.handle = None;
    }
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
        let (pixels, [w, h]) = build_globe_image(&globe, true, WORLD_MAP_OVERSAMPLE);
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels);
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

/// Render the globe as an RGBA pixel buffer. `oversample` is pixels per
/// climate cell (1 = legacy block-colour render, 2+ = bilinear sub-cell
/// detail matching the in-game `biome::classify_at_tile` field). When
/// `respect_fog` is true, unexplored cells render as dark grey
/// (player-facing in-game map). Returns `(pixels, [width, height])`.
pub fn build_globe_image(
    globe: &Globe,
    respect_fog: bool,
    oversample: u32,
) -> (Vec<u8>, [usize; 2]) {
    let oversample = oversample.max(1);
    let img_w = GLOBE_WIDTH as u32 * oversample;
    let img_h = GLOBE_HEIGHT as u32 * oversample;
    let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];

    let inv_os = 1.0 / oversample as f32;

    for py in 0..img_h {
        for px in 0..img_w {
            let idx = ((py * img_w + px) * 4) as usize;

            // Fractional climate-cell coords for bilinear sampling.
            let fx = (px as f32 + 0.5) * inv_os;
            let fy = (py as f32 + 0.5) * inv_os;
            let gx = (fx as i32).clamp(0, GLOBE_WIDTH - 1);
            let gy = (fy as i32).clamp(0, GLOBE_HEIGHT - 1);
            let cell = globe.cell(gx, gy).unwrap();

            let rgba = if respect_fog && !cell.explored {
                [25, 25, 25, 255]
            } else {
                // Bilinear-classify at oversampled pixel position + sample
                // the bilinear elevation field for hillshading. The same
                // climate field as `classify_at_tile`, evaluated at the
                // pixel's fractional cell location → smooth biome boundaries
                // that match the actual in-game terrain instead of blocky
                // per-cell colours.
                let tiles_per_cell = (GLOBE_CELL_CHUNKS
                    * crate::world::chunk::CHUNK_SIZE as i32)
                    as f32;
                let tile_x = (fx * tiles_per_cell) as i32;
                let tile_y = (fy * tiles_per_cell) as i32;
                let (elev_u, _, _) = globe.sample_climate(tile_x, tile_y);
                let elev_f = (elev_u / 255.0).clamp(0.0, 1.0);

                let mut c = if oversample > 1 {
                    crate::world::biome::classify_at_tile(globe, tile_x, tile_y)
                        .color()
                } else {
                    cell.biome.color()
                };

                // Hillshade: brightness scales with elevation so altitude is
                // visible alongside biome. Deep ocean reads dark, mountain
                // peaks read bright; mid-elev grassland sits near 1.0.
                // Skip on lake cells (overpainted blue below). Rivers no
                // longer use cell-level tinting — they get a polyline
                // overlay after the main pass that follows the actual
                // curving channel rather than the blocky climate cell.
                if !cell.is_lake {
                    let shade = 0.55 + 0.95 * elev_f; // [0.55, 1.50]
                    c[0] = ((c[0] as f32 * shade).clamp(0.0, 255.0)) as u8;
                    c[1] = ((c[1] as f32 * shade).clamp(0.0, 255.0)) as u8;
                    c[2] = ((c[2] as f32 * shade).clamp(0.0, 255.0)) as u8;
                }

                if cell.is_lake {
                    c[0] = 30;
                    c[1] = 80;
                    c[2] = 180;
                }
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

    // ── River polyline overlay ────────────────────────────────────────────
    // Walk every river edge's tile polyline, project to image-pixel coords,
    // and Bresenham-draw it. Width tapers from `edge.from_width` to
    // `edge.to_width` along arc length so trunks read fatter than tributary
    // headwaters. Drawing happens pre-flip so the orientation matches the
    // biome buffer; respect_fog hides rivers in unexplored cells.
    let tiles_per_cell = (GLOBE_CELL_CHUNKS * crate::world::chunk::CHUNK_SIZE as i32) as f32;
    let world_to_px = |tile_x: i32, tile_y: i32| -> (i32, i32) {
        let px = ((tile_x as f32 / tiles_per_cell) * oversample as f32) as i32;
        let py = ((tile_y as f32 / tiles_per_cell) * oversample as f32) as i32;
        (px, py)
    };
    for (edge_idx, edge) in globe.rivers.edges.iter().enumerate() {
        let Some(polyline) = globe.rivers.edge_polylines.get(edge_idx) else {
            continue;
        };
        if polyline.len() < 2 {
            continue;
        }
        // Cumulative arc length for taper.
        let mut lens = Vec::with_capacity(polyline.len());
        lens.push(0.0f32);
        for w in polyline.windows(2) {
            let dx = (w[1].0 - w[0].0) as f32;
            let dy = (w[1].1 - w[0].1) as f32;
            let prev = *lens.last().unwrap();
            lens.push(prev + (dx * dx + dy * dy).sqrt());
        }
        let total = lens.last().copied().unwrap_or(1.0).max(1.0);
        for i in 0..polyline.len() - 1 {
            let (ax, ay) = polyline[i];
            let (bx, by) = polyline[i + 1];
            let t_mid = (lens[i] + lens[i + 1]) * 0.5 / total;
            let mid_w = edge.from_width as f32
                + (edge.to_width as f32 - edge.from_width as f32) * t_mid;
            // Per-pixel half-width: minor streams 1px, major rivers up to 3px.
            let half_px = (mid_w * 0.5).round() as i32;
            let half_px = half_px.clamp(0, 3);
            let (px0, py0) = world_to_px(ax, ay);
            let (px1, py1) = world_to_px(bx, by);
            // Bresenham; stamp a small chebyshev square at each pixel.
            let dx = (px1 - px0).abs();
            let sx = if px0 < px1 { 1 } else { -1 };
            let dy = -(py1 - py0).abs();
            let sy = if py0 < py1 { 1 } else { -1 };
            let mut err = dx + dy;
            let mut x = px0;
            let mut y = py0;
            loop {
                for oy in -half_px..=half_px {
                    for ox in -half_px..=half_px {
                        let qx = x + ox;
                        let qy = y + oy;
                        if qx < 0 || qy < 0 || qx >= img_w as i32 || qy >= img_h as i32 {
                            continue;
                        }
                        if respect_fog {
                            // Project pixel back to climate cell to gate on explored.
                            let cgx = ((qx as f32 + 0.5) / oversample as f32) as i32;
                            let cgy = ((qy as f32 + 0.5) / oversample as f32) as i32;
                            if let Some(cell) = globe.cell(cgx, cgy) {
                                if !cell.explored {
                                    continue;
                                }
                            }
                        }
                        let idx = ((qy * img_w as i32 + qx) * 4) as usize;
                        pixels[idx] = 60;
                        pixels[idx + 1] = 130;
                        pixels[idx + 2] = 200;
                        pixels[idx + 3] = 255;
                    }
                }
                if x == px1 && y == py1 {
                    break;
                }
                let e2 = 2 * err;
                if e2 >= dy {
                    err += dy;
                    x += sx;
                }
                if e2 <= dx {
                    err += dx;
                    y += sy;
                }
            }
        }
    }

    // Flip Y so north is up (Bevy Y=0 is bottom).
    let row_bytes = (img_w * 4) as usize;
    let mut flipped = vec![0u8; pixels.len()];
    for row in 0..img_h as usize {
        let src = row * row_bytes;
        let dst = (img_h as usize - 1 - row) * row_bytes;
        flipped[dst..dst + row_bytes].copy_from_slice(&pixels[src..src + row_bytes]);
    }
    (flipped, [img_w as usize, img_h as usize])
}
