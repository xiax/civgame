use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::person::Person;
use crate::simulation::schedule::SimClock;
use crate::economy::mode::EconomicMode;
use crate::world::seasons::Calendar;

pub fn hud_system(
    mut contexts: EguiContexts,
    mut clock: ResMut<SimClock>,
    mut mode: ResMut<EconomicMode>,
    calendar: Res<Calendar>,
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

                        ui.label(
                            egui::RichText::new("Speed:")
                                .color(egui::Color32::WHITE),
                        );
                        if ui.button("1×").clicked() { clock.speed = 1.0; }
                        if ui.button("2×").clicked() { clock.speed = 2.0; }
                        if ui.button("5×").clicked() { clock.speed = 5.0; }

                        ui.separator();
                        if ui.button(mode.label()).clicked() {
                            *mode = mode.cycle();
                        }

                        ui.separator();
                        let season_color = match calendar.season {
                            crate::world::seasons::Season::Spring => egui::Color32::from_rgb(100, 220, 100),
                            crate::world::seasons::Season::Summer => egui::Color32::from_rgb(255, 220, 50),
                            crate::world::seasons::Season::Autumn => egui::Color32::from_rgb(230, 130, 30),
                            crate::world::seasons::Season::Winter => egui::Color32::from_rgb(180, 220, 255),
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
                    });
                });
        });
}
