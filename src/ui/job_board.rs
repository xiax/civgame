use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::faction::{FactionRegistry, PlayerFaction};
use crate::economy::goods::Good;
use crate::simulation::jobs::{
    JobBoard, JobBoardCommand, JobKind, JobPosting, JobProgress, JobSource, TileAabb,
    PLAYER_PRIORITY,
};
use crate::simulation::projects::{
    ProjectCancelReason, ProjectEventKind, ProjectPhase, Projects,
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
            draft_kind: JobKind::Stockpile,
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
    registry: Res<FactionRegistry>,
    projects: Res<Projects>,
    mut commands: EventWriter<JobBoardCommand>,
) {
    let ctx = contexts.ctx_mut();
    let mut open = state.open;
    egui::Window::new("Faction Jobs")
        .open(&mut open)
        .default_pos(egui::pos2(20.0, 360.0))
        .default_width(280.0)
        .show(ctx, |ui| {
            // Workforce budget bars + actual claim counts.
            if let Some(faction) = registry.factions.get(&player_faction.faction_id) {
                let budget = faction.workforce_budget;
                let postings = board.faction_postings(player_faction.faction_id);
                // Indices: 0=stockpile_food, 1=stockpile_wood, 2=stockpile_stone,
                // 3=haul, 4=farm, 5=build, 6=craft
                let mut claims = [0u32; 7];
                for p in postings {
                    let i = match (&p.kind, &p.progress) {
                        (JobKind::Stockpile, JobProgress::Stockpile { good, .. }) => match good {
                            Good::Wood => 1,
                            Good::Stone => 2,
                            _ => 0,
                        },
                        (JobKind::Stockpile, _) => 0,
                        (JobKind::Haul, _) => 3,
                        (JobKind::Farm, _) => 4,
                        (JobKind::Build, _) => 5,
                        (JobKind::Craft, _) => 6,
                    };
                    claims[i] += p.claimants.len() as u32;
                }
                let pop = faction.member_count.max(1) as f32;
                ui.collapsing("Workforce Budget", |ui| {
                    let row = |ui: &mut egui::Ui, label: &str, share: f32, claimed: u32| {
                        let cap = (share * pop).round().max(1.0) as u32;
                        ui.horizontal(|ui| {
                            ui.label(format!("{:<14}", label));
                            ui.add(egui::ProgressBar::new(share).desired_width(120.0));
                            ui.label(format!("{}/{}", claimed, cap));
                        });
                    };
                    row(ui, "Stockpile Food",  budget.stockpile_food,  claims[0]);
                    row(ui, "Stockpile Wood",  budget.stockpile_wood,  claims[1]);
                    row(ui, "Stockpile Stone", budget.stockpile_stone, claims[2]);
                    row(ui, "Haul",            budget.haul,            claims[3]);
                    row(ui, "Farm",            budget.farm,            claims[4]);
                    row(ui, "Build",           budget.build,           claims[5]);
                    row(ui, "Craft",           budget.craft,           claims[6]);
                    ui.horizontal(|ui| {
                        ui.label(format!("{:<14}", "Free"));
                        ui.add(egui::ProgressBar::new(budget.free).desired_width(120.0));
                    });
                });

                // Active projects summary.
                let project_list: Vec<&crate::simulation::projects::Project> = projects
                    .faction_projects(player_faction.faction_id)
                    .collect();
                if !project_list.is_empty() {
                    ui.collapsing(format!("Projects ({})", project_list.len()), |ui| {
                        for project in project_list {
                            let phase = match project.phase {
                                ProjectPhase::GatherMaterials => "Gather",
                                ProjectPhase::Build => "Build",
                            };
                            ui.label(format!("#{:?} — {}", project.blueprint, phase));
                        }
                    });
                }
                // Recent project lifecycle events (cancellations, downgrades).
                let recent_events: Vec<_> = projects
                    .recent_events
                    .iter()
                    .rev()
                    .filter(|e| e.faction_id == player_faction.faction_id)
                    .take(8)
                    .collect();
                if !recent_events.is_empty() {
                    ui.collapsing("Recent project events", |ui| {
                        for event in recent_events {
                            let label = match &event.kind {
                                ProjectEventKind::Cancelled { reason } => match reason {
                                    ProjectCancelReason::StalledGather { good } => format!(
                                        "t{} cancelled — stalled gathering {:?}",
                                        event.tick, good
                                    ),
                                },
                            };
                            ui.label(label);
                        }
                    });
                }
                ui.separator();
            }

            // Post Job form.
            ui.collapsing("Post Job", |ui| {
                egui::ComboBox::from_label("Kind")
                    .selected_text(state.draft_kind.name())
                    .show_ui(ui, |ui| {
                        for kind in [JobKind::Stockpile, JobKind::Farm, JobKind::Craft]
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
        JobProgress::Stockpile {
            good,
            deposited,
            target,
        } => format!("Stockpile {:?} {}/{}", good, deposited, target),
        JobProgress::Haul {
            good,
            delivered,
            target,
            ..
        } => format!("Haul {:?} {}/{}", good, delivered, target),
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
        JobKind::Stockpile => JobProgress::Calories {
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
        // Build and Haul postings need a concrete blueprint; the Post Job form
        // doesn't pick one yet — disabled at this layer.
        JobKind::Build | JobKind::Haul => return None,
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
