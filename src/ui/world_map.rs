use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::region::{average_fertility_in_megachunk, MegaChunkCoord, SettledRegions};
use crate::world::globe::{
    Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH, MEGACHUNK_SIZE_CHUNKS,
};

/// Pixels per climate cell when rendering the world map. 4× combines with
/// the surface-biome warp (see `biome::classify_surface_at_tile`) and the
/// LINEAR-filtered texture upload below so sub-cell biome borders render
/// organic/feathered at the 10× display scale instead of stair-stepping.
/// Above 4× is cosmetic — the warp's lattice is the data resolution.
pub const WORLD_MAP_OVERSAMPLE: u32 = 4;

#[derive(Resource, Default)]
pub struct WorldMapOpen(pub bool);

/// User-toggleable overlays on the world map.
#[derive(Resource, Default)]
pub struct WorldMapView {
    /// Tint every mega-chunk by its expected average fertility (climate-derived).
    pub show_fertility: bool,
    /// Tint mega-chunks by faction territory ownership (settlement +
    /// camp anchors → `TerritoryMap`). Materialised factions only;
    /// abstract-faction Globe cells continue to render via the regular
    /// settled-region outline.
    pub show_territory: bool,
}

/// Cache so we don't re-upload the texture every frame unless globe changed.
#[derive(Resource, Default)]
pub struct WorldMapTexture {
    handle: Option<egui::TextureHandle>,
    /// Last count of explored cells when we built the texture — rebuild when it changes.
    last_explored: u32,
    /// Whether the cached texture has the fertility overlay baked in.
    last_show_fertility: bool,
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
    mut view: ResMut<WorldMapView>,
    globe: Res<Globe>,
    mut tex_cache: ResMut<WorldMapTexture>,
    mut settled: ResMut<SettledRegions>,
    mut camera_q: Query<&mut Transform, With<Camera>>,
    mut camera_state: ResMut<crate::rendering::camera::CameraState>,
    view_projection: crate::rendering::projection::ViewProjection,
    territory: Res<crate::simulation::territory::TerritoryMap>,
) {
    // Suppress camera input while map is open
    if open.0 {
        camera_state.drag_origin = None;
    }

    if !open.0 {
        return;
    }

    let ctx = contexts.ctx_mut();

    // Rebuild texture if explored cells changed or the fertility toggle flipped.
    let explored_count = globe.cells.iter().filter(|c| c.explored).count() as u32;
    if tex_cache.handle.is_none()
        || tex_cache.last_explored != explored_count
        || tex_cache.last_show_fertility != view.show_fertility
    {
        let (pixels, [w, h]) =
            build_globe_image(&globe, true, WORLD_MAP_OVERSAMPLE, view.show_fertility);
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels);
        // LINEAR filtering at 10× display scale lets the surface-biome
        // warp + oversample-4 buffer read as smooth feathered borders;
        // 1px grid / river overlays soften slightly (they're already
        // drawn faintly into the buffer pre-upload).
        let handle = ctx.load_texture("world_map", image, egui::TextureOptions::LINEAR);
        tex_cache.handle = Some(handle);
        tex_cache.last_explored = explored_count;
        tex_cache.last_show_fertility = view.show_fertility;
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
            ui.horizontal(|ui| {
                ui.checkbox(&mut view.show_fertility, "Show fertility");
                ui.label(
                    egui::RichText::new("(green = high, brown = low; estimated from climate)")
                        .small()
                        .weak(),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut view.show_territory, "Show territory");
                ui.label(
                    egui::RichText::new("(coloured by owning faction; sparse overlay over loaded anchors)")
                        .small()
                        .weak(),
                );
            });

            // Scale image to fit within ~800×400 px
            let scale = 10.0f32;
            let img_w = GLOBE_WIDTH as f32 * scale;
            let img_h = GLOBE_HEIGHT as f32 * scale;

            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(img_w, img_h), egui::Sense::click());

            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            use crate::world::chunk::CHUNK_SIZE;
            use crate::world::terrain::TILE_SIZE;

            let total_tiles_x = (GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;
            let total_tiles_y = (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32;

            // Mega-chunk overlay: outline every settled mega-chunk in faction colour.
            let mc_grid_w = (GLOBE_WIDTH * GLOBE_CELL_CHUNKS) / MEGACHUNK_SIZE_CHUNKS;
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
                ui.painter()
                    .rect_stroke(r, 0.0, egui::Stroke::new(2.0, color));
            }

            // Territory overlay: paint mega-chunks coloured by the
            // dominant `TerritoryMap` faction owner in that mega-chunk.
            // Sparse — only mega-chunks with at least one claimed tile
            // contribute, so abstract-faction regions stay uncoloured
            // here (they're rendered by the settled-region outline pass).
            if view.show_territory && !territory.cells.is_empty() {
                use ahash::AHashMap;
                let mut mc_owner: AHashMap<(i32, i32), AHashMap<u32, u32>> =
                    AHashMap::default();
                for (&tile, cell) in &territory.cells {
                    let Some(owner) = cell.owner else {
                        continue;
                    };
                    let mc = MegaChunkCoord::from_tile(tile.0, tile.1);
                    *mc_owner
                        .entry(mc)
                        .or_default()
                        .entry(owner)
                        .or_insert(0) += 1;
                }
                for ((mx, my), per_faction) in mc_owner {
                    if mx < 0 || my < 0 || mx >= mc_grid_w || my >= mc_grid_h {
                        continue;
                    }
                    let Some((winner, _)) =
                        per_faction.into_iter().max_by_key(|(_, n)| *n)
                    else {
                        continue;
                    };
                    let screen_my = mc_grid_h - 1 - my;
                    let r = egui::Rect::from_min_size(
                        egui::pos2(
                            rect.min.x + mx as f32 * cell_w,
                            rect.min.y + screen_my as f32 * cell_h,
                        ),
                        egui::vec2(cell_w, cell_h),
                    );
                    let color = territory_tint_for_faction(winner);
                    ui.painter().rect_filled(r, 0.0, color);
                }
            }

            // Draw current camera position as a white rectangle. Convert
            // through `camera_view_to_logical` so the marker tracks the
            // logical (top-down) tile under the camera in tilted mode.
            if let Ok(cam_t) = camera_q.get_single() {
                let logical = view_projection.camera_to_logical(cam_t.translation.truncate());
                let tile_x = logical.x / TILE_SIZE;
                let tile_y = logical.y / TILE_SIZE;

                let nx = tile_x / total_tiles_x;
                let ny = 1.0 - tile_y / total_tiles_y;

                use crate::world::chunk_streaming::LOAD_RADIUS;
                let view_w = (LOAD_RADIUS * 2) as f32 / (GLOBE_WIDTH * GLOBE_CELL_CHUNKS) as f32;
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

            // Hover tooltip: show mega-chunk coords + biome + avg fertility.
            if let Some(pos) = response.hover_pos() {
                if rect.contains(pos) {
                    let mx = ((pos.x - rect.min.x) / cell_w) as i32;
                    let screen_my = ((pos.y - rect.min.y) / cell_h) as i32;
                    let my = mc_grid_h - 1 - screen_my;
                    let mx = mx.clamp(0, mc_grid_w - 1);
                    let my = my.clamp(0, mc_grid_h - 1);
                    let (tx, ty) = MegaChunkCoord::center_tile(mx, my);
                    // Surface-biome layer so the tooltip names the biome
                    // the user sees rendered (canonical `classify_at_tile`
                    // is reserved for AI / salinity / world-sim).
                    let biome = crate::world::biome::classify_surface_at_tile(&globe, tx, ty);
                    let fert = average_fertility_in_megachunk(&globe, mx, my);
                    let dominant_relief =
                        crate::world::geomorph::dominant_relief_in_megachunk(&globe.relief, mx, my);
                    let center_relief = globe.sample_relief(tx, ty);
                    let settled_here = settled.by_megachunk.contains_key(&(mx, my));
                    egui::show_tooltip(
                        ctx,
                        ui.layer_id(),
                        egui::Id::new("world_map_tooltip"),
                        |ui| {
                            ui.label(format!("Mega-chunk ({}, {})", mx, my));
                            ui.label(format!("Centre biome: {}", biome.name()));
                            if let Some(r) = dominant_relief {
                                ui.label(format!("Dominant relief: {}", r.name()));
                            }
                            ui.label(format!(
                                "Centre relief: {} (slope {:.0}%)",
                                center_relief.class.name(),
                                center_relief.slope * 100.0,
                            ));
                            ui.label(format!("Avg fertility: {}/255", fert));
                            if settled_here {
                                ui.colored_label(
                                    egui::Color32::from_rgb(255, 220, 100),
                                    "Settled — click to jump.",
                                );
                            }
                        },
                    );
                }
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
                            let cur_logical =
                                view_projection.camera_to_logical(cam_t.translation.truncate());
                            let cur_tx = (cur_logical.x / TILE_SIZE).floor() as i32;
                            let cur_ty = (cur_logical.y / TILE_SIZE).floor() as i32;
                            let cur_mc = MegaChunkCoord::from_tile(cur_tx, cur_ty);
                            if let Some(&cur_region_id) = settled.by_megachunk.get(&cur_mc) {
                                if let Some(cr) = settled.by_id.get_mut(&cur_region_id) {
                                    // Bookmarks always store logical
                                    // coords so they survive mode toggles.
                                    cr.camera_bookmark = cur_logical;
                                }
                            }
                            // Jump to the target region's bookmark — bookmark
                            // is logical, project it to the camera's view space.
                            if let Some(target_region) = settled.by_id.get(&region_id) {
                                let view = view_projection
                                    .logical_to_camera(target_region.camera_bookmark);
                                cam_t.translation.x = view.x;
                                cam_t.translation.y = view.y;
                            }
                        }
                    }
                }
            }
        });
}

/// Render the globe as an RGBA pixel buffer. `oversample` is pixels per
/// climate cell (1 = legacy block-colour from stored `cell.biome`; 2+ =
/// per-pixel `biome::classify_surface_at_tile`, the same surface-biome
/// layer chunk-gen uses, so preview matches generated terrain). When
/// `respect_fog` is true, unexplored cells render as dark grey
/// (player-facing in-game map). When `show_fertility` is true, each
/// mega-chunk is tinted by `average_fertility_in_megachunk` (climate-derived)
/// after biome/hillshade/lake/faction passes but before the river polyline
/// overlay, so coastlines and rivers still read clearly. Returns
/// `(pixels, [width, height])`.
pub fn build_globe_image(
    globe: &Globe,
    respect_fog: bool,
    oversample: u32,
    show_fertility: bool,
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
                // Surface-biome layer (domain-warped land biomes, true-elev
                // Ocean/Mountain gates) so preview matches what chunk-gen
                // would stamp at the same tile. Pure fn of (globe.seed,
                // tile_x, tile_y) — no `WorldGen` required.
                let tiles_per_cell =
                    (GLOBE_CELL_CHUNKS * crate::world::chunk::CHUNK_SIZE as i32) as f32;
                let tile_x = (fx * tiles_per_cell) as i32;
                let tile_y = (fy * tiles_per_cell) as i32;
                let (elev_u, _, _) = globe.sample_climate(tile_x, tile_y);
                let elev_f = (elev_u / 255.0).clamp(0.0, 1.0);

                let mut c = if oversample > 1 {
                    crate::world::biome::classify_surface_at_tile(globe, tile_x, tile_y).color()
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

    // ── Fertility overlay (optional) ──────────────────────────────────────
    // Tint each mega-chunk by its expected average fertility, blended at ~65%
    // alpha so coast/river overlays drawn after still read clearly. The
    // climate estimator is chunk-independent so this works without any
    // chunks loaded — see `average_fertility_in_megachunk`.
    if show_fertility {
        let mc_grid_w = (GLOBE_WIDTH * GLOBE_CELL_CHUNKS) / MEGACHUNK_SIZE_CHUNKS;
        let mc_grid_h = (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS) / MEGACHUNK_SIZE_CHUNKS;
        // Mega-chunk pixel side = climate-cells-per-megachunk * oversample.
        let mc_cells = MEGACHUNK_SIZE_CHUNKS / GLOBE_CELL_CHUNKS;
        let mc_px_w = (mc_cells as u32 * oversample) as i32;
        let mc_px_h = mc_px_w;
        let alpha = 0.65f32;
        for my in 0..mc_grid_h {
            for mx in 0..mc_grid_w {
                let fert = average_fertility_in_megachunk(globe, mx, my);
                let (fr, fg, fb) = fertility_ramp_color(fert);
                let x0 = mx * mc_px_w;
                let y0 = my * mc_px_h;
                let x1 = (x0 + mc_px_w).min(img_w as i32);
                let y1 = (y0 + mc_px_h).min(img_h as i32);
                for py in y0..y1 {
                    for px in x0..x1 {
                        if respect_fog {
                            let cgx = ((px as f32 + 0.5) / oversample as f32) as i32;
                            let cgy = ((py as f32 + 0.5) / oversample as f32) as i32;
                            if let Some(cell) = globe.cell(cgx, cgy) {
                                if !cell.explored {
                                    continue;
                                }
                            }
                        }
                        let idx = ((py * img_w as i32 + px) * 4) as usize;
                        let r = pixels[idx] as f32;
                        let g = pixels[idx + 1] as f32;
                        let b = pixels[idx + 2] as f32;
                        pixels[idx] = (r * (1.0 - alpha) + fr as f32 * alpha) as u8;
                        pixels[idx + 1] = (g * (1.0 - alpha) + fg as f32 * alpha) as u8;
                        pixels[idx + 2] = (b * (1.0 - alpha) + fb as f32 * alpha) as u8;
                    }
                }
            }
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
            let mid_w =
                edge.from_width as f32 + (edge.to_width as f32 - edge.from_width as f32) * t_mid;
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

/// Two-stop colour ramp for the fertility overlay: dark brown (0) → tan (~128)
/// → deep green (255). Returns sRGB bytes.
/// Deterministic faction-id → tint for the territory overlay. ~50%
/// alpha so the biome texture stays legible underneath.
fn territory_tint_for_faction(faction_id: u32) -> egui::Color32 {
    // Cheap deterministic palette: hash → HSV-ish via three integer
    // splays. Avoids pulling in a colour crate.
    let h = faction_id.wrapping_mul(0x9E37_79B9);
    let r = ((h >> 0) & 0xFF) as u8;
    let g = ((h >> 8) & 0xFF) as u8;
    let b = ((h >> 16) & 0xFF) as u8;
    // Lift each channel into the 80-255 band so dark colours don't
    // blend invisibly into the biome map.
    let lift = |c: u8| 80 + ((c as u16 * 175 / 255) as u8);
    egui::Color32::from_rgba_premultiplied(lift(r), lift(g), lift(b), 120)
}

fn fertility_ramp_color(fert: u8) -> (u8, u8, u8) {
    let t = fert as f32 / 255.0;
    if t < 0.5 {
        let k = t * 2.0;
        let r = lerp_u8(70, 200, k);
        let g = lerp_u8(45, 175, k);
        let b = lerp_u8(30, 90, k);
        (r, g, b)
    } else {
        let k = (t - 0.5) * 2.0;
        let r = lerp_u8(200, 30, k);
        let g = lerp_u8(175, 140, k);
        let b = lerp_u8(90, 40, k);
        (r, g, b)
    }
}

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::globe::generate_globe;

    #[test]
    fn world_map_image_dimensions_scale_with_oversample() {
        let g = generate_globe(42);
        let (pixels, [w, h]) = build_globe_image(&g, false, WORLD_MAP_OVERSAMPLE, false);
        assert_eq!(w, (GLOBE_WIDTH as u32 * WORLD_MAP_OVERSAMPLE) as usize);
        assert_eq!(h, (GLOBE_HEIGHT as u32 * WORLD_MAP_OVERSAMPLE) as usize);
        assert_eq!(pixels.len(), w * h * 4);
        // At oversample 4 we expect non-trivial pixel count.
        assert!(pixels.len() >= 1_000_000);
    }

    #[test]
    fn world_map_image_shows_multiple_biome_colors() {
        // Surface-biome render at oversample > 1 should produce a varied
        // palette across a globe-sized image. Bucket pixels into RGB
        // octants and require several distinct buckets — sanity that the
        // classifier isn't collapsed to a single colour.
        let g = generate_globe(42);
        let (pixels, [w, h]) = build_globe_image(&g, false, WORLD_MAP_OVERSAMPLE, false);
        let mut buckets = std::collections::HashSet::new();
        // Stride sample so this stays fast.
        let stride = ((w * h) / 4_000).max(1);
        for i in (0..(w * h)).step_by(stride) {
            let idx = i * 4;
            let r = pixels[idx] / 64;
            let gg = pixels[idx + 1] / 64;
            let b = pixels[idx + 2] / 64;
            buckets.insert((r, gg, b));
        }
        assert!(
            buckets.len() >= 4,
            "expected several biome colours, got {}",
            buckets.len()
        );
    }
}
