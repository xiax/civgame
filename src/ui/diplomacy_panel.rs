//! Diplomacy panel — faction list, reputation tracks, pending
//! proposals, and outgoing-action buttons. Player-facing surface for
//! the `DiplomacyLedger`.
//!
//! Toggled by the HUD's "Diplomacy" button (`DiplomacyPanelOpen`).
//! Read-only when no player faction exists.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::diplomacy::{
    DiplomacyLedger, DiplomacyProposal, ProposalResponse, TreatyKind,
};
use crate::simulation::diplomatic_contact::DiplomaticContactBook;
use crate::simulation::diplomatic_evaluator::{
    evaluate_proposal_v2, FairnessLabel, Perspective,
};
use crate::simulation::diplomatic_personality::DiplomaticPersonality;
use crate::simulation::faction::{FactionRegistry, PlayerFaction, SOLO};
use crate::simulation::player_command::{CommandSender, PlayerCommand};

#[derive(Resource, Default)]
pub struct DiplomacyPanelOpen(pub bool);

/// Persistent selection between renders so the right pane stays on
/// one faction across frames.
#[derive(Resource, Default)]
pub struct DiplomacyPanelSelection {
    pub focused_faction: Option<u32>,
}

pub fn diplomacy_panel_system(
    mut contexts: EguiContexts,
    mut open: ResMut<DiplomacyPanelOpen>,
    mut selection: ResMut<DiplomacyPanelSelection>,
    ledger: Res<DiplomacyLedger>,
    registry: Res<FactionRegistry>,
    contact_book: Res<DiplomaticContactBook>,
    player_faction: Res<PlayerFaction>,
    mut sender: CommandSender,
) {
    if !open.0 {
        return;
    }
    let self_fid = player_faction.faction_id;
    if self_fid == SOLO {
        return;
    }
    let Some(self_data) = registry.factions.get(&self_fid) else {
        return;
    };
    let self_root = registry.root_faction(self_fid);

    // Smart-diplomacy P1 — gate the list on `DiplomaticContactBook`. A
    // faction the player has never contacted shouldn't appear (drops
    // omniscient pre-meeting visibility). We also surface any faction
    // we *currently have an open proposal/ledger relation with* —
    // first-contact via diplomatic mail counts as known.
    let mut foreign: Vec<u32> = registry
        .factions
        .iter()
        .filter(|(fid, data)| {
            **fid != SOLO
                && **fid != self_fid
                && data.parent_faction.is_none()
                && registry.root_faction(**fid) != self_root
                && (contact_book.is_known(self_root, registry.root_faction(**fid))
                    || ledger
                        .relation(self_fid, **fid)
                        .map(|r| !r.incident_log.is_empty() || r.last_contact_tick > 0)
                        .unwrap_or(false))
        })
        .map(|(fid, _)| *fid)
        .collect();
    foreign.sort_unstable();

    egui::Window::new("Diplomacy")
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(640.0)
        .default_height(420.0)
        .resizable(true)
        .show(contexts.ctx_mut(), |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Your faction: {} (treasury {:.1})",
                        self_fid, self_data.treasury
                    ))
                    .strong(),
                );
                if ui.button("Close").clicked() {
                    open.0 = false;
                }
            });
            ui.separator();
            ui.columns(2, |cols| {
                // ── Left: faction list ───────────────────────────
                cols[0].label(egui::RichText::new("Known factions").strong());
                cols[0].separator();
                egui::ScrollArea::vertical()
                    .id_source("dipl_left_scroll")
                    .max_height(360.0)
                    .show(&mut cols[0], |ui| {
                        if foreign.is_empty() {
                            ui.label(egui::RichText::new("(none known)").italics());
                        }
                        for &target in &foreign {
                            let rel = ledger.relation(self_fid, target);
                            let (attitude, treaty_summary) = rel
                                .map(|r| {
                                    (
                                        r.reputation.attitude_label(r.treaties),
                                        treaty_summary(r.treaties),
                                    )
                                })
                                .unwrap_or(("Neutral", String::from("—")));
                            let label = format!(
                                "Faction {} — {} [{}]",
                                target, attitude, treaty_summary
                            );
                            let is_focused = selection.focused_faction == Some(target);
                            if ui.selectable_label(is_focused, label).clicked() {
                                selection.focused_faction = Some(target);
                            }
                        }
                    });

                // ── Right: detail + actions ──────────────────────
                let focused = selection
                    .focused_faction
                    .or_else(|| foreign.first().copied());
                let Some(target) = focused else {
                    cols[1].label("Select a faction on the left.");
                    return;
                };
                cols[1].label(
                    egui::RichText::new(format!("Faction {}", target)).strong(),
                );
                cols[1].separator();
                let relation = ledger.relation(self_fid, target);
                if let Some(r) = relation {
                    cols[1].label(format!(
                        "Trust: {}   Fear: {}   Grievance: {}   Familiarity: {}",
                        r.reputation.trust,
                        r.reputation.fear,
                        r.reputation.grievance,
                        r.reputation.familiarity
                    ));
                    cols[1].label(format!("Treaties: {}", treaty_summary(r.treaties)));
                    cols[1].label(format!(
                        "Last contact: tick {}",
                        r.last_contact_tick
                    ));
                    cols[1].label(
                        egui::RichText::new("Recent incidents")
                            .strong(),
                    );
                    egui::ScrollArea::vertical()
                        .id_source("dipl_incidents")
                        .max_height(120.0)
                        .show(&mut cols[1], |ui| {
                            for inc in r.incident_log.iter().rev() {
                                ui.label(format!("  t={} — {:?}", inc.tick, inc.kind));
                            }
                        });
                } else {
                    cols[1].label("(no contact yet)");
                }

                cols[1].separator();
                cols[1].label(egui::RichText::new("Pending proposals").strong());
                let inbox: Vec<_> = ledger
                    .inbox_by_faction
                    .get(&self_fid)
                    .map(|v| v.clone())
                    .unwrap_or_default();
                let mut any_for_target = false;
                for pid in &inbox {
                    let Some(p) = ledger.proposals.get(pid) else {
                        continue;
                    };
                    if p.from_faction != target {
                        continue;
                    }
                    any_for_target = true;
                    // Smart-diplomacy P1 — score the proposal from the
                    // *player's* (receiver) perspective so the panel
                    // can surface a one-word fairness label.
                    let fairness_str = self_data.parent_faction.is_none().then(|| {
                        let pers = DiplomaticPersonality::from_culture(
                            &self_data.culture,
                            self_data.caps.home.is_mobile(),
                        );
                        let relation = ledger
                            .relation(p.from_faction, self_fid)
                            .cloned()
                            .unwrap_or_default();
                        let contact = contact_book.record_of(self_root, registry.root_faction(p.from_faction));
                        let util = evaluate_proposal_v2(
                            p.proposal,
                            &relation,
                            &pers,
                            self_data.home_tile,
                            contact,
                            Perspective::Receiver,
                        );
                        fairness_color_label(util.fairness)
                    });
                    cols[1].horizontal(|ui| {
                        ui.label(format!("{:?}", p.proposal));
                        if let Some((label, color)) = fairness_str {
                            ui.label(egui::RichText::new(format!("[{}]", label)).color(color));
                        }
                        if ui.button("Accept").clicked() {
                            sender.send(
                                Vec::new(),
                                PlayerCommand::RespondDiplomacyProposal {
                                    faction_id: self_fid,
                                    proposal_id: *pid,
                                    response: ProposalResponse::Accept,
                                },
                            );
                        }
                        if ui.button("Reject").clicked() {
                            sender.send(
                                Vec::new(),
                                PlayerCommand::RespondDiplomacyProposal {
                                    faction_id: self_fid,
                                    proposal_id: *pid,
                                    response: ProposalResponse::Reject,
                                },
                            );
                        }
                    });
                }
                if !any_for_target {
                    cols[1].label("(no pending proposals from this faction)");
                }

                cols[1].separator();
                cols[1].label(egui::RichText::new("Actions").strong());
                let treaties = relation.map(|r| r.treaties).unwrap_or_default();
                let at_war = treaties.has(TreatyKind::War);
                cols[1].horizontal_wrapped(|ui| {
                    let propose = |kind: &str, proposal: DiplomacyProposal, ui: &mut egui::Ui,
                                   sender: &mut CommandSender|
                     -> bool {
                        let clicked = ui.button(kind).clicked();
                        if clicked {
                            sender.send(
                                Vec::new(),
                                PlayerCommand::SendDiplomacyProposal {
                                    faction_id: self_fid,
                                    target_faction_id: target,
                                    proposal,
                                },
                            );
                        }
                        clicked
                    };
                    if at_war {
                        propose("Offer Peace", DiplomacyProposal::OfferPeace, ui, &mut sender);
                    } else {
                        if !treaties.has(TreatyKind::TradePact) {
                            propose(
                                "Offer Trade Pact",
                                DiplomacyProposal::OfferTradePact,
                                ui,
                                &mut sender,
                            );
                        }
                        if !treaties.has(TreatyKind::Alliance) {
                            propose(
                                "Offer Alliance",
                                DiplomacyProposal::OfferAlliance,
                                ui,
                                &mut sender,
                            );
                        }
                        if !treaties.has(TreatyKind::NonAggression) {
                            propose(
                                "Offer Non-Aggression",
                                DiplomacyProposal::OfferNonAggression,
                                ui,
                                &mut sender,
                            );
                        }
                        if ui
                            .button(egui::RichText::new("Declare War").color(egui::Color32::from_rgb(220, 60, 60)))
                            .clicked()
                        {
                            sender.send(
                                Vec::new(),
                                PlayerCommand::DeclareWar {
                                    faction_id: self_fid,
                                    target_faction_id: target,
                                },
                            );
                        }
                    }
                });
                if !at_war {
                    cols[1].horizontal_wrapped(|ui| {
                        for kind in [
                            TreatyKind::TradePact,
                            TreatyKind::Alliance,
                            TreatyKind::NonAggression,
                        ] {
                            if treaties.has(kind) && ui.button(format!("Break {:?}", kind)).clicked() {
                                sender.send(
                                    Vec::new(),
                                    PlayerCommand::BreakTreaty {
                                        faction_id: self_fid,
                                        target_faction_id: target,
                                        treaty: kind,
                                    },
                                );
                            }
                        }
                    });
                }
            });
        });
}

fn fairness_color_label(f: FairnessLabel) -> (&'static str, egui::Color32) {
    match f {
        FairnessLabel::Generous => ("Generous", egui::Color32::from_rgb(120, 220, 140)),
        FairnessLabel::Fair => ("Fair", egui::Color32::from_rgb(220, 220, 220)),
        FairnessLabel::HardBargain => ("Hard Bargain", egui::Color32::from_rgb(220, 180, 90)),
        FairnessLabel::Exploitative => ("Bad Deal", egui::Color32::from_rgb(220, 80, 80)),
    }
}

fn treaty_summary(t: crate::simulation::diplomacy::TreatySet) -> String {
    let mut parts: Vec<&'static str> = Vec::new();
    if t.has(TreatyKind::War) {
        parts.push("War");
    }
    if t.has(TreatyKind::Alliance) {
        parts.push("Ally");
    }
    if t.has(TreatyKind::TradePact) {
        parts.push("Trade");
    }
    if t.has(TreatyKind::NonAggression) {
        parts.push("NAP");
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join("+")
    }
}
