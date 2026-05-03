use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use std::collections::VecDeque;

use crate::simulation::construction::BuildSiteKind;
use crate::simulation::faction::PlayerFaction;

const MAX_ENTRIES: usize = 16;
const TICKS_PER_DAY: u64 = 3600;
const DAYS_PER_SEASON: u64 = 5;


#[derive(Event, Clone)]
pub struct ActivityLogEvent {
    pub tick: u64,
    pub actor: Entity,
    pub faction_id: u32,
    pub kind: ActivityEntryKind,
}

#[derive(Clone)]
pub enum ActivityEntryKind {
    Constructed {
        site: BuildSiteKind,
        tile: (i32, i32),
        result_entity: Entity,
    },
    Crafted {
        name: &'static str,
    },
    TechDiscovered {
        tech_name: &'static str,
        era_name: &'static str,
    },
    RegionSettled {
        megachunk: (i32, i32),
        region_name: String,
    },
}

#[derive(Clone)]
pub enum ResultLink {
    Built {
        entity: Entity,
        snapshot: Vec2,
    },
    HeldByActor,
    NoTarget,
}

#[derive(Clone)]
pub struct ActivityLogEntry {
    pub tick: u64,
    pub actor: Entity,
    pub actor_snapshot: Option<Vec2>,
    pub verb: &'static str,
    pub thing_label: String,
    pub result: ResultLink,
}

#[derive(Resource, Default)]
pub struct ActivityLog {
    pub entries: VecDeque<ActivityLogEntry>,
}

#[derive(Resource, Default)]
pub struct CameraFocusRequest(pub Option<Vec2>);

pub fn activity_log_ingest_system(
    mut events: EventReader<ActivityLogEvent>,
    mut log: ResMut<ActivityLog>,
    transforms: Query<&Transform>,
    player_faction: Res<PlayerFaction>,
) {
    for ev in events.read() {
        if ev.faction_id != player_faction.faction_id {
            continue;
        }

        let actor_snapshot = transforms
            .get(ev.actor)
            .ok()
            .map(|t| t.translation.truncate());

        let (verb, thing_label, result) = match &ev.kind {
            ActivityEntryKind::RegionSettled { megachunk, region_name } => {
                (
                    "settled",
                    format!("{} (mega-chunk {:?})", region_name, megachunk),
                    ResultLink::NoTarget,
                )
            }
            &ActivityEntryKind::Constructed {
                site,
                tile,
                result_entity,
            } => {
                let world = transforms
                    .get(result_entity)
                    .map(|t| t.translation.truncate())
                    .unwrap_or_else(|_| {
                        crate::world::terrain::tile_to_world(tile.0 as i32, tile.1 as i32)
                    });
                (
                    "built",
                    site.label().to_string(),
                    ResultLink::Built {
                        entity: result_entity,
                        snapshot: world,
                    },
                )
            }
            &ActivityEntryKind::Crafted { name } => (
                "crafted",
                name.to_string(),
                ResultLink::HeldByActor,
            ),
            &ActivityEntryKind::TechDiscovered { tech_name, era_name } => (
                "discovered",
                format!("{} ({})", tech_name, era_name),
                ResultLink::NoTarget,
            ),
        };

        log.entries.push_back(ActivityLogEntry {
            tick: ev.tick,
            actor: ev.actor,
            actor_snapshot,
            verb,
            thing_label,
            result,
        });
        while log.entries.len() > MAX_ENTRIES {
            log.entries.pop_front();
        }
    }
}

pub fn activity_log_panel_system(
    mut contexts: EguiContexts,
    mut log: ResMut<ActivityLog>,
    mut selected: ResMut<crate::ui::selection::SelectedEntity>,
    mut focus: ResMut<CameraFocusRequest>,
    name_query: Query<&Name>,
    transforms: Query<&Transform>,
) {
    if log.entries.is_empty() {
        return;
    }

    let ctx = contexts.ctx_mut();

    egui::Area::new(egui::Id::new("activity_log"))
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0])
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.set_min_width(280.0);
                    ui.set_max_width(320.0);

                    let mut clear = false;
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Activity")
                                .color(egui::Color32::from_rgb(220, 220, 220))
                                .strong(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Clear").clicked() {
                                clear = true;
                            }
                        });
                    });
                    ui.separator();

                    let mut click_actor: Option<Entity> = None;
                    let mut click_result_entity: Option<Entity> = None;
                    let mut click_focus: Option<Vec2> = None;

                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for entry in log.entries.iter() {
                                let is_tech = entry.actor == Entity::PLACEHOLDER;

                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;

                                    if is_tech {
                                        ui.label(
                                            egui::RichText::new("★")
                                                .color(egui::Color32::from_rgb(255, 210, 80)),
                                        );
                                    } else {
                                        let actor_alive = name_query.get(entry.actor).is_ok();
                                        let actor_name = name_query
                                            .get(entry.actor)
                                            .map(|n| n.as_str().to_string())
                                            .unwrap_or_else(|_| "(gone)".to_string());

                                        let actor_btn = link_button(ui, &actor_name, actor_alive);
                                        if actor_btn.clicked() && actor_alive {
                                            click_actor = Some(entry.actor);
                                            click_focus = transforms
                                                .get(entry.actor)
                                                .ok()
                                                .map(|t| t.translation.truncate())
                                                .or(entry.actor_snapshot);
                                        }
                                    }

                                    ui.label(
                                        egui::RichText::new(entry.verb)
                                            .color(egui::Color32::from_rgb(180, 180, 180)),
                                    );

                                    match &entry.result {
                                        ResultLink::Built { entity, snapshot } => {
                                            let result_alive = transforms.get(*entity).is_ok();
                                            let thing_btn = link_button(
                                                ui,
                                                &entry.thing_label,
                                                result_alive,
                                            );
                                            if thing_btn.clicked() && result_alive {
                                                click_result_entity = Some(*entity);
                                                click_focus = transforms
                                                    .get(*entity)
                                                    .ok()
                                                    .map(|t| t.translation.truncate())
                                                    .or(Some(*snapshot));
                                            }
                                        }
                                        ResultLink::HeldByActor => {
                                            let actor_alive =
                                                name_query.get(entry.actor).is_ok();
                                            let thing_btn = link_button(
                                                ui,
                                                &entry.thing_label,
                                                actor_alive,
                                            );
                                            if thing_btn.clicked() && actor_alive {
                                                click_actor = Some(entry.actor);
                                                click_focus = transforms
                                                    .get(entry.actor)
                                                    .ok()
                                                    .map(|t| t.translation.truncate())
                                                    .or(entry.actor_snapshot);
                                            }
                                        }
                                        ResultLink::NoTarget => {
                                            ui.label(
                                                egui::RichText::new(&entry.thing_label)
                                                    .color(egui::Color32::from_rgb(
                                                        200, 230, 200,
                                                    )),
                                            );
                                        }
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(tick_to_game_date(entry.tick))
                                                    .small()
                                                    .color(egui::Color32::from_rgb(140, 140, 140)),
                                            );
                                        },
                                    );
                                });
                            }
                        });

                    if clear {
                        log.entries.clear();
                    }
                    if let Some(e) = click_result_entity {
                        selected.0 = Some(e);
                    } else if let Some(e) = click_actor {
                        selected.0 = Some(e);
                    }
                    if let Some(p) = click_focus {
                        focus.0 = Some(p);
                    }
                });
        });

}

pub fn camera_focus_system(
    mut req: ResMut<CameraFocusRequest>,
    mut cam: Query<&mut Transform, With<Camera2d>>,
) {
    let Some(target) = req.0.take() else {
        return;
    };
    let Ok(mut tf) = cam.get_single_mut() else {
        return;
    };
    tf.translation.x = target.x;
    tf.translation.y = target.y;
}

fn link_button(ui: &mut egui::Ui, text: &str, alive: bool) -> egui::Response {
    let color = if alive {
        egui::Color32::from_rgb(120, 200, 255)
    } else {
        egui::Color32::from_rgb(120, 120, 120)
    };
    let rich = egui::RichText::new(text).color(color).underline();
    ui.add(egui::Button::new(rich).frame(false))
}

fn tick_to_game_date(tick: u64) -> String {
    let total_days = tick / TICKS_PER_DAY;
    let day = total_days % DAYS_PER_SEASON + 1;
    let season_abbr = ["Spr", "Sum", "Aut", "Win"][(total_days / DAYS_PER_SEASON % 4) as usize];
    format!("{} D{}", season_abbr, day)
}
