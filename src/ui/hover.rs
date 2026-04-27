use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::{egui, EguiContexts};

use crate::economy::agent::EconomicAgent;
use crate::simulation::animals::{AnimalAI, Deer, Wolf};
use crate::simulation::combat::{Body, Health};
use crate::simulation::faction::FactionMember;
use crate::simulation::items::GroundItem;
use crate::simulation::mood::Mood;
use crate::simulation::needs::Needs;
use crate::simulation::person::{Person, PersonAI};
use crate::simulation::plants::Plant;
use crate::simulation::reproduction::BiologicalSex;
use crate::world::chunk::ChunkMap;
use crate::world::globe::Globe;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_at_3d, world_to_tile, WorldGen};

pub fn hover_info_system(
    mut contexts: EguiContexts,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera>>,
    chunk_map: Res<ChunkMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    spatial_index: Res<SpatialIndex>,
    person_query: Query<
        (
            &PersonAI,
            &Needs,
            &Mood,
            &BiologicalSex,
            &FactionMember,
            &EconomicAgent,
            &Body,
        ),
        With<Person>,
    >,
    animal_query: Query<(&AnimalAI, &Health, Option<&Wolf>, Option<&Deer>)>,
    plant_query: Query<&Plant>,
    item_query: Query<&GroundItem>,
    name_query: Query<&Name>,
) {
    let Ok(window) = windows.get_single() else {
        return;
    };
    let Ok((camera, cam_transform)) = camera_query.get_single() else {
        return;
    };

    let ctx = contexts.ctx_mut();
    if ctx.is_pointer_over_area() || ctx.wants_pointer_input() {
        return;
    }

    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };
    let Ok(world_pos) = camera.viewport_to_world_2d(cam_transform, cursor_pos) else {
        return;
    };
    let (tx, ty) = world_to_tile(world_pos);

    let tooltip_id = egui::Id::new("hover_tooltip");
    egui::show_tooltip_at_pointer(
        ctx,
        egui::LayerId::debug(),
        tooltip_id,
        |ui: &mut egui::Ui| {
            ui.label(format!("Tile: ({}, {})", tx, ty));

            let surf_z = chunk_map.surface_z_at(tx, ty);
            if surf_z >= crate::world::chunk::Z_MIN {
                let tile = tile_at_3d(&chunk_map, &gen, &globe, tx, ty, surf_z);
                ui.label(format!("Kind: {:?}", tile.kind));
                ui.label(format!("Z: {}", surf_z));
                ui.label(format!("Fertility: {}", tile.fertility));
                if tile.has_building() {
                    ui.label("Has Building");
                }
            } else {
                ui.label("Unloaded Chunk");
            }

            let entities = spatial_index.get(tx, ty);
            if !entities.is_empty() {
                ui.separator();
                ui.label("Entities:");
                for &entity in entities {
                    if let Ok((ai, needs, mood, sex, faction, agent, body)) =
                        person_query.get(entity)
                    {
                        let name = name_query
                            .get(entity)
                            .map(|n| n.as_str())
                            .unwrap_or("Person");
                        ui.collapsing(format!("{} ({:?})", name, sex), |ui| {
                            ui.label(format!("Health: {:.0}%", body.fraction() * 100.0));
                            ui.label(format!("State: {:?}", ai.state));
                            ui.label(format!("Faction: {}", faction.faction_id));
                            ui.label(format!("Mood: {} ({})", mood.label(), mood.0));
                            ui.label(format!("Hunger: {}", needs.hunger));
                            ui.label(format!("Sleep: {}", needs.sleep));
                            ui.label(format!("Currency: {:.1}", agent.currency));
                            ui.label("Inventory:");
                            for (item, qty) in agent.inventory {
                                if qty > 0 {
                                    ui.label(format!("  - {:?} x{}", item.good, qty));
                                }
                            }
                        });
                    } else if let Ok((ai, health, wolf, deer)) = animal_query.get(entity) {
                        let kind = if wolf.is_some() {
                            "Wolf"
                        } else if deer.is_some() {
                            "Deer"
                        } else {
                            "Animal"
                        };
                        ui.label(format!("{}: {:?}", kind, ai.state));
                        ui.label(format!("Health: {}/{}", health.current, health.max));
                    } else if let Ok(plant) = plant_query.get(entity) {
                        ui.label(format!("Plant: {:?} ({:?})", plant.kind, plant.stage));
                    } else if let Ok(item) = item_query.get(entity) {
                        ui.label(format!("Item: {:?} x{}", item.item.good, item.qty));
                    } else {
                        ui.label(format!("Entity: {:?}", entity));
                    }
                }
            }
        },
    );
}
