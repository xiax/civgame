use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::faction::PlayerFaction;
use crate::simulation::jobs::{
    JobBoard, JobBoardCommand, JobKind, JobPosting, JobProgress, JobSource, TileAabb,
    PLAYER_PRIORITY,
};
use crate::simulation::schedule::SimClock;

/// State persisted between frames for the player-side Post Job form.
#[derive(Resource)]
pub struct JobBoardPanelState {
    pub open: bool,
    pub draft_kind: JobKind,
    pub draft_target: u32,
    pub draft_radius: i16,
}

impl Default for JobBoardPanelState {
    fn default() -> Self {
        Self {
            open: true,
            draft_kind: JobKind::Gather,
            draft_target: 200,
            draft_radius: 5,
        }
    }
}

pub fn job_board_panel_system(
    mut contexts: EguiContexts,
    mut state: ResMut<JobBoardPanelState>,
    board: Res<JobBoard>,
    clock: Res<SimClock>,
    player_faction: Res<PlayerFaction>,
    mut commands: EventWriter<JobBoardCommand>,
) {
    let ctx = contexts.ctx_mut();
    let mut open = state.open;
    egui::Window::new("Faction Jobs")
        .open(&mut open)
        .default_pos(egui::pos2(20.0, 360.0))
        .default_width(280.0)
        .show(ctx, |ui| {
            // Post Job form.
            ui.collapsing("Post Job", |ui| {
                egui::ComboBox::from_label("Kind")
                    .selected_text(state.draft_kind.name())
                    .show_ui(ui, |ui| {
                        for kind in [JobKind::Gather, JobKind::Farm, JobKind::Craft, JobKind::Build]
                        {
                            ui.selectable_value(&mut state.draft_kind, kind, kind.name());
                        }
                    });
                ui.horizontal(|ui| {
                    ui.label("Target");
                    ui.add(
                        egui::DragValue::new(&mut state.draft_target)
                            .clamp_range(1..=5000)
                            .speed(1),
                    );
                });
                if matches!(state.draft_kind, JobKind::Farm) {
                    ui.horizontal(|ui| {
                        ui.label("Radius");
                        ui.add(
                            egui::DragValue::new(&mut state.draft_radius)
                                .clamp_range(1..=20)
                                .speed(1),
                        );
                    });
                }
                if ui.button("Post").clicked() {
                    if let Some(posting) = build_player_posting(
                        state.draft_kind,
                        state.draft_target,
                        state.draft_radius,
                        player_faction.faction_id,
                        clock.tick as u32,
                    ) {
                        commands.send(JobBoardCommand::Post(posting));
                    }
                }
            });
            ui.separator();

            // List the player faction's postings.
            let postings = board.faction_postings(player_faction.faction_id);
            if postings.is_empty() {
                ui.label("No active postings.");
            } else {
                let mut to_cancel: Option<u32> = None;
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for p in postings {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(p.kind.name());
                                ui.label(format!("[{}]", source_label(p.source)));
                                ui.label(format!("p{}", p.priority));
                            });
                            ui.label(progress_label(&p.progress));
                            let frac = p.progress.fraction();
                            ui.add(egui::ProgressBar::new(frac).desired_width(220.0));
                            ui.horizontal(|ui| {
                                ui.label(format!("Workers: {}", p.claimants.len()));
                                if ui.button("Cancel").clicked() {
                                    to_cancel = Some(p.id);
                                }
                            });
                        });
                    }
                });
                if let Some(id) = to_cancel {
                    commands.send(JobBoardCommand::Cancel(id));
                }
            }
        });
    state.open = open;
}

fn source_label(s: JobSource) -> &'static str {
    match s {
        JobSource::Chief => "Chief",
        JobSource::Player => "Player",
    }
}

fn progress_label(p: &JobProgress) -> String {
    match p {
        JobProgress::Calories { deposited, target } => {
            format!("Calories {}/{}", deposited, target)
        }
        JobProgress::Material {
            good,
            delivered,
            target,
        } => format!("{:?} {}/{}", good, delivered, target),
        JobProgress::Planting {
            planted, target, ..
        } => format!("Tiles planted {}/{}", planted, target),
        JobProgress::Crafting {
            crafted,
            target,
            recipe,
            ..
        } => format!("Recipe #{} {}/{}", recipe, crafted, target),
        JobProgress::Building { .. } => "Build in progress".to_string(),
    }
}

fn build_player_posting(
    kind: JobKind,
    target: u32,
    radius: i16,
    faction_id: u32,
    posted_tick: u32,
) -> Option<JobPosting> {
    let progress = match kind {
        JobKind::Gather => JobProgress::Calories {
            deposited: 0,
            target,
        },
        JobKind::Farm => JobProgress::Planting {
            planted: 0,
            target,
            // Without a tile picker, fall back to a placeholder area centered
            // at origin; a future pass will hook this to the right-click menu.
            area: TileAabb {
                min: (-radius, -radius),
                max: (radius, radius),
            },
        },
        JobKind::Craft => JobProgress::Crafting {
            crafted: 0,
            target,
            recipe: 0,
            bench: None,
        },
        // Build postings need a concrete blueprint; the Post Job form doesn't
        // pick one yet — disabled at this layer.
        JobKind::Build => return None,
    };
    Some(JobPosting {
        id: 0, // overwritten by job_board_command_system
        faction_id,
        kind,
        progress,
        claimants: Vec::new(),
        priority: PLAYER_PRIORITY,
        source: JobSource::Player,
        posted_tick,
        expiry_tick: Some(posted_tick + 600),
    })
}
