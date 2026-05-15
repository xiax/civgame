use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::egui;

use crate::economy::agent::EconomicAgent;
use crate::simulation::animals::{AnimalAI, Deer, Wolf};
use crate::simulation::carry::Carrier;
use crate::simulation::combat::{Body, Health};
use crate::simulation::construction::{
    recipe_for, AutonomousBuildingToggle, Blueprint, BlueprintMap, StructureIndex, StructureLabel,
};
use crate::simulation::crafting::{craft_recipes, CraftOrder, CraftOrderMap};

#[derive(SystemParam)]
pub struct SitesHoverParams<'w, 's> {
    pub bp_map: Res<'w, BlueprintMap>,
    pub auto_build: Res<'w, AutonomousBuildingToggle>,
    pub bp_query: Query<'w, 's, &'static Blueprint>,
    pub co_map: Res<'w, CraftOrderMap>,
    pub co_query: Query<'w, 's, &'static CraftOrder>,
    pub structure_index: Res<'w, StructureIndex>,
    pub structure_label_q: Query<'w, 's, &'static StructureLabel>,
    pub deployable_q: Query<'w, 's, &'static crate::simulation::pack_deploy::Deployable>,
}
use crate::simulation::faction::FactionMember;
use crate::simulation::items::GroundItem;
use crate::simulation::land::{Plot, PlotIndex, Tenure, TenureHolder};
use crate::simulation::lod::LodLevel;
use crate::simulation::mood::Mood;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plants::Plant;
use crate::simulation::reproduction::BiologicalSex;
use crate::simulation::tasks::TaskKind;
use crate::world::chunk::ChunkMap;
use crate::world::globe::Globe;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_at_3d, world_to_tile, WorldGen};

pub fn hover_info_system(
    mut cursor: crate::rendering::projection::CursorParams,
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
    animal_query: Query<(
        &AnimalAI,
        &Health,
        Option<&Wolf>,
        Option<&Deer>,
        Option<&crate::simulation::animals::Tamed>,
        Option<&crate::simulation::animals::PackAnimalInventory>,
    )>,
    plant_query: Query<&Plant>,
    item_query: Query<&GroundItem>,
    name_query: Query<&Name>,
    sites: SitesHoverParams,
    worker_query: Query<
        (
            &FactionMember,
            &PersonAI,
            &crate::simulation::typed_task::ActionQueue,
            &EconomicAgent,
            &Carrier,
            &LodLevel,
            &Transform,
        ),
        With<Person>,
    >,
    plot_index: Res<PlotIndex>,
    plot_query: Query<&Plot>,
) {
    let Some(pick) = cursor.cursor_pick() else {
        return;
    };
    let (tx, ty) = pick.tile;
    let ctx = cursor.contexts.ctx_mut();

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

            if let Some(pid) = plot_index.plot_at(tx as i32, ty as i32) {
                if let Some(&entity) = plot_index.by_id.get(&pid) {
                    if let Ok(plot) = plot_query.get(entity) {
                        ui.separator();
                        ui.label(egui::RichText::new("Plot").strong());
                        ui.label(format!(
                            "ID: {}  ·  Settlement: {}",
                            plot.id, plot.settlement_id
                        ));
                        ui.label(format!("Zone: {}", plot.zone_kind.label()));
                        ui.label(format!(
                            "Rect: ({}, {})  {}×{}",
                            plot.rect.x0, plot.rect.y0, plot.rect.w, plot.rect.h
                        ));
                        let tenure = match plot.tenure {
                            Tenure::StateOwned => "State-owned".to_string(),
                            Tenure::Leased { rent_per_month, .. } => {
                                format!("Leased ({:.1}/mo)", rent_per_month)
                            }
                            Tenure::Sharecropping {
                                share_to_landlord, ..
                            } => {
                                format!(
                                    "Sharecropping ({:.0}% to landlord)",
                                    share_to_landlord * 100.0
                                )
                            }
                            Tenure::Freehold => "Freehold".to_string(),
                        };
                        let holder = match plot.holder {
                            TenureHolder::State { faction_id } => {
                                format!("State (faction {})", faction_id)
                            }
                            TenureHolder::Household { faction_id } => {
                                format!("Household {}", faction_id)
                            }
                        };
                        ui.label(format!("Tenure: {}", tenure));
                        ui.label(format!("Holder: {}", holder));
                        if plot.base_value > 0.0 {
                            ui.label(format!("Value: {:.1}", plot.base_value));
                        }
                        if let (Some(edge), Some(at)) = (plot.frontage_edge, plot.access_tile) {
                            ui.label(format!("Frontage: {:?} → ({}, {})", edge, at.0, at.1));
                        }
                        if let Some(parent) = plot.parent_plot {
                            ui.label(format!("Child of plot #{}", parent));
                        }
                    }
                }
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
                            ui.label(format!("Thirst: {}", needs.thirst));
                            ui.label(format!("Sleep: {}", needs.sleep));
                            ui.label(format!("Currency: {:.1}", agent.currency));
                            ui.label("Inventory:");
                            for (item, qty) in agent.inventory {
                                if qty > 0 {
                                    ui.label(format!(
                                        "  - {} x{}",
                                        crate::economy::core_ids::display_name(item.resource_id),
                                        qty
                                    ));
                                }
                            }
                        });
                    } else if let Ok((ai, health, wolf, deer, tamed, pack)) =
                        animal_query.get(entity)
                    {
                        let kind = if wolf.is_some() {
                            "Wolf"
                        } else if deer.is_some() {
                            "Deer"
                        } else {
                            "Animal"
                        };
                        ui.label(format!("{}: {:?}", kind, ai.state));
                        ui.label(format!("Health: {}/{}", health.current, health.max));
                        if let Some(t) = tamed {
                            ui.label(format!("Tamed by: faction {}", t.owner_faction));
                        }
                        // P8: pack animal cargo. Surface what each animal
                        // is hauling so the player can audit migration
                        // packing and see why a horse is "loaded".
                        if let Some(inv) = pack {
                            ui.label(format!(
                                "Carrying: {} / {} kg",
                                inv.current_weight_g() / 1000,
                                inv.capacity_g / 1000,
                            ));
                            for (rid, qty) in inv.iter() {
                                ui.label(format!(
                                    "  - {} x{}",
                                    crate::economy::core_ids::display_name(rid),
                                    qty,
                                ));
                            }
                        }
                    } else if let Ok(plant) = plant_query.get(entity) {
                        let threshold =
                            crate::simulation::plants::stage_threshold(plant.kind, plant.stage);
                        if threshold == 0 {
                            ui.label(format!("Plant: {:?} ({:?})", plant.kind, plant.stage));
                        } else {
                            ui.label(format!(
                                "Plant: {:?} ({:?}) — growth {}/{}",
                                plant.kind, plant.stage, plant.growth, threshold
                            ));
                        }
                    } else if let Ok(item) = item_query.get(entity) {
                        ui.label(format!(
                            "Item: {} x{}",
                            crate::economy::core_ids::display_name(item.item.resource_id),
                            item.qty
                        ));
                    } else {
                        ui.label(format!("Entity: {:?}", entity));
                    }
                }
            }

            let tile_key = (tx as i32, ty as i32);
            if let Some(&structure_entity) = sites.structure_index.0.get(&tile_key) {
                if let Ok(label) = sites.structure_label_q.get(structure_entity) {
                    ui.separator();
                    ui.label(egui::RichText::new(label.0).strong());
                    // Surface nomadic-shelter pack/refund semantics on
                    // hover so the player knows what migrating will cost
                    // them. Bedrolls + Yurts pack into inventory; Tents
                    // refund half their wood as ground items.
                    if let Ok(deployable) = sites.deployable_q.get(structure_entity) {
                        if let Some(packed) = deployable.packed_form {
                            let name = crate::economy::core_ids::display_name(packed);
                            ui.label(
                                egui::RichText::new(format!("Packs as: {} on migration", name))
                                    .small()
                                    .weak(),
                            );
                        } else if let Some((rid, qty)) = deployable.compute_refund_drop() {
                            let name = crate::economy::core_ids::display_name(rid);
                            ui.label(
                                egui::RichText::new(format!(
                                    "Teardown: drops {} {} on migration ({}% refund)",
                                    qty,
                                    name,
                                    (deployable.refund_pct * 100.0) as i32,
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    }
                }
            }

            let bp_key = (tx as i32, ty as i32);
            if let Some(&bp_entity) = sites.bp_map.0.get(&bp_key) {
                if let Ok(bp) = sites.bp_query.get(bp_entity) {
                    ui.separator();
                    let recipe = recipe_for(bp.kind);
                    ui.label(egui::RichText::new(format!("Blueprint: {}", recipe.name)).strong());
                    ui.label(format!("Kind: {:?}", bp.kind));
                    ui.label(format!("Faction: {}", bp.faction_id));
                    if let Some(owner) = bp.personal_owner {
                        ui.label(format!("Personal owner: {:?}", owner));
                    }
                    ui.label(format!("Target Z: {}", bp.target_z));
                    ui.label(format!(
                        "Progress: {} / {}",
                        bp.build_progress, recipe.work_ticks
                    ));
                    ui.label(format!(
                        "Satisfied: {}",
                        if bp.is_satisfied() { "yes" } else { "no" }
                    ));
                    ui.label(format!("AutoBuild: {}", sites.auto_build.0));

                    ui.label("Deposits:");
                    for i in 0..bp.deposit_count as usize {
                        let d = bp.deposits[i];
                        let line = format!(
                            "  {}: {}/{}",
                            crate::economy::core_ids::display_name(d.resource_id),
                            d.deposited,
                            d.needed
                        );
                        if d.deposited < d.needed {
                            ui.label(egui::RichText::new(line).color(egui::Color32::LIGHT_RED));
                        } else {
                            ui.label(line);
                        }
                    }

                    // Worker / hauler diagnostic block.
                    let mut on_site_workers = 0u32;
                    let mut on_site_haulers = 0u32;
                    let mut idle_nearby = 0u32;
                    let mut closest_dormant: Option<i32> = None;
                    // Per-deposit-slot carrier counts.
                    let mut slot_carriers = [0u32; 3];

                    let bp_pos = (tx, ty);
                    for (member, ai, aq, agent, carrier, lod, transform) in worker_query.iter() {
                        let allowed = match bp.personal_owner {
                            Some(_) => false, // personal blueprint — only owner counts
                            None => member.faction_id == bp.faction_id,
                        };
                        // For personal blueprints we still want to show owner state;
                        // the entity-id match is checked via spatial lookup below.
                        if !allowed && bp.personal_owner.is_none() {
                            continue;
                        }

                        // On-site count: target_entity points at this bp AND task matches.
                        if ai.target_entity == Some(bp_entity) {
                            let t = aq.current_task_kind();
                            if t == TaskKind::Construct as u16 || t == TaskKind::ConstructBed as u16
                            {
                                on_site_workers += 1;
                            } else if t == TaskKind::HaulMaterials as u16 {
                                on_site_haulers += 1;
                            }
                        }

                        if !allowed {
                            continue;
                        }

                        let agent_world = transform.translation.truncate();
                        let (atx, aty) = world_to_tile(agent_world);
                        let dist = (atx as i32 - bp_pos.0).abs() + (aty as i32 - bp_pos.1).abs();

                        if *lod == LodLevel::Dormant {
                            if closest_dormant.map_or(true, |d| dist < d) {
                                closest_dormant = Some(dist);
                            }
                            continue;
                        }

                        if ai.state == AiState::Idle && dist <= 30 {
                            idle_nearby += 1;
                        }

                        for i in 0..bp.deposit_count as usize {
                            let need = bp.deposits[i];
                            if need.deposited >= need.needed {
                                continue;
                            }
                            let qty = carrier.quantity_of_resource(need.resource_id)
                                + agent.quantity_of_resource(need.resource_id);
                            if qty > 0 {
                                slot_carriers[i] += 1;
                            }
                        }
                    }

                    ui.separator();
                    ui.label(egui::RichText::new("Diagnostics").strong());
                    let on_site_line = format!(
                        "On-site: {} worker(s), {} hauler(s)",
                        on_site_workers, on_site_haulers
                    );
                    if on_site_workers + on_site_haulers == 0 {
                        ui.label(egui::RichText::new(on_site_line).color(egui::Color32::LIGHT_RED));
                    } else {
                        ui.label(on_site_line);
                    }
                    let idle_line = format!("Eligible idle nearby (≤30): {}", idle_nearby);
                    if idle_nearby == 0 {
                        ui.label(egui::RichText::new(idle_line).color(egui::Color32::LIGHT_RED));
                    } else {
                        ui.label(idle_line);
                    }
                    ui.label("Carriers per unmet slot:");
                    for i in 0..bp.deposit_count as usize {
                        let need = bp.deposits[i];
                        if need.deposited >= need.needed {
                            continue;
                        }
                        let line = format!(
                            "  {} carriers: {}",
                            crate::economy::core_ids::display_name(need.resource_id),
                            slot_carriers[i]
                        );
                        if slot_carriers[i] == 0 {
                            ui.label(egui::RichText::new(line).color(egui::Color32::LIGHT_RED));
                        } else {
                            ui.label(line);
                        }
                    }
                    if let Some(d) = closest_dormant {
                        ui.label(format!("Closest dormant member: {} tiles", d));
                    }
                }
            }

            // Craft Order at this tile (anchor / workbench).
            let co_key = (tx as i32, ty as i32);
            if let Some(&co_entity) = sites.co_map.0.get(&co_key) {
                if let Ok(order) = sites.co_query.get(co_entity) {
                    ui.separator();
                    let recipe_name = craft_recipes()
                        .get(order.recipe_id as usize)
                        .map(|r| r.name)
                        .unwrap_or("?");
                    let work_ticks = craft_recipes()
                        .get(order.recipe_id as usize)
                        .map(|r| r.work_ticks)
                        .unwrap_or(0);
                    ui.label(egui::RichText::new(format!("Craft Order: {}", recipe_name)).strong());
                    ui.label(format!("Faction: {}", order.faction_id));
                    if let Some((wbx, wby)) = order.workbench_tile {
                        ui.label(format!("Workbench: ({}, {})", wbx, wby));
                    } else {
                        ui.label("Stationless (anchored at camp)");
                    }
                    ui.label(format!("Work: {} / {}", order.work_progress, work_ticks));
                    ui.label(format!(
                        "Satisfied: {}",
                        if order.is_satisfied() { "yes" } else { "no" }
                    ));
                    ui.label("Deposits:");
                    for i in 0..order.deposit_count as usize {
                        let d = order.deposits[i];
                        let line = format!(
                            "  {}: {}/{}",
                            crate::economy::core_ids::display_name(d.resource_id),
                            d.deposited,
                            d.needed
                        );
                        if d.deposited < d.needed {
                            ui.label(egui::RichText::new(line).color(egui::Color32::LIGHT_RED));
                        } else {
                            ui.label(line);
                        }
                    }
                }
            }
        },
    );
}
