use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::mode::EconomicMode;
use crate::rendering::camera::CameraViewZ;
use crate::simulation::construction::AutonomousBuildingToggle;
use crate::simulation::faction::{FactionRegistry, PlayerFaction};
use crate::ui::tech_panel::TechPanelOpen;
use crate::simulation::person::Person;
use crate::simulation::schedule::SimClock;
use crate::world::seasons::Calendar;

pub fn hud_system(
    mut contexts: EguiContexts,
    mut clock: ResMut<SimClock>,
    mut mode: ResMut<EconomicMode>,
    mut auto_build: ResMut<AutonomousBuildingToggle>,
    mut tech_panel_open: ResMut<TechPanelOpen>,
    camera_view_z: Res<CameraViewZ>,
    calendar: Res<Calendar>,
    player_faction: Res<PlayerFaction>,
    registry: Res<FactionRegistry>,
    persons: Query<(), With<Person>>,
) {
    let pop = persons.iter().count();

    egui::Area::new(egui::Id::new("hud"))
        .fixed_pos([0.0, 0.0])
        .show(contexts.ctx_mut(), |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Population: {pop}"))
                                .color(egui::Color32::WHITE)
                                .size(16.0),
                        );
                        ui.separator();

                        ui.label(egui::RichText::new("Speed:").color(egui::Color32::WHITE));

                        let active_fill = egui::Color32::from_rgb(60, 120, 200);
                        for (label, target) in
                            [("⏸", 0.0_f32), ("1×", 1.0), ("2×", 2.0), ("5×", 5.0)]
                        {
                            let btn = egui::Button::new(label);
                            let btn = if (clock.speed - target).abs() < 0.01 {
                                btn.fill(active_fill)
                            } else {
                                btn
                            };
                            if ui.add(btn).clicked() {
                                clock.speed = target;
                            }
                        }

                        ui.separator();
                        if ui.button(mode.label()).clicked() {
                            *mode = mode.cycle();
                        }

                        ui.separator();
                        let build_lbl = if auto_build.0 {
                            "Build: ON"
                        } else {
                            "Build: OFF"
                        };
                        let build_btn = egui::Button::new(build_lbl).fill(if auto_build.0 {
                            egui::Color32::from_rgb(60, 180, 80)
                        } else {
                            egui::Color32::from_gray(60)
                        });
                        if ui.add(build_btn).clicked() {
                            auto_build.0 = !auto_build.0;
                        }

                        ui.separator();
                        let tech_btn = egui::Button::new("Tech").fill(if tech_panel_open.0 {
                            egui::Color32::from_rgb(140, 100, 200)
                        } else {
                            egui::Color32::from_gray(60)
                        });
                        if ui.add(tech_btn).clicked() {
                            tech_panel_open.0 = !tech_panel_open.0;
                        }

                        ui.separator();
                        let season_color = match calendar.season {
                            crate::world::seasons::Season::Spring => {
                                egui::Color32::from_rgb(100, 220, 100)
                            }
                            crate::world::seasons::Season::Summer => {
                                egui::Color32::from_rgb(255, 220, 50)
                            }
                            crate::world::seasons::Season::Autumn => {
                                egui::Color32::from_rgb(230, 130, 30)
                            }
                            crate::world::seasons::Season::Winter => {
                                egui::Color32::from_rgb(180, 220, 255)
                            }
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{} Day {}",
                                calendar.season.name(),
                                calendar.day
                            ))
                            .color(season_color),
                        );

                        ui.separator();
                        ui.label(
                            egui::RichText::new(format!("Tick: {}", clock.tick))
                                .color(egui::Color32::GRAY),
                        );

                        ui.separator();
                        let z_label = if camera_view_z.0 == i32::MAX {
                            "Z: Surface".to_string()
                        } else {
                            format!("Z: {} (PgUp/PgDn)", camera_view_z.0)
                        };
                        let z_color = if camera_view_z.0 == i32::MAX {
                            egui::Color32::GRAY
                        } else {
                            egui::Color32::from_rgb(255, 200, 80)
                        };
                        ui.label(egui::RichText::new(z_label).color(z_color));
                    });

                    let fid = player_faction.faction_id;
                    if fid != 0 {
                        if let Some(data) = registry.factions.get(&fid) {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Your Faction #{fid}:  food: {:.1}  |  members: {}",
                                        data.storage.food_total(), data.member_count
                                    ))
                                    .color(egui::Color32::from_rgb(140, 215, 255)),
                                );
                            });
                        }
                    }
                });
        });
}
