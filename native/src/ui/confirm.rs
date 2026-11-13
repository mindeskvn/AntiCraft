//! Tool-call confirmation as an inline card at the end of the transcript (DESIGN.md 2.5):
//! colored unified diff for edits, command preview for shell, a short summary for
//! read/search tools when auto-approve is off. Enter approves once, Esc rejects (when no
//! text field has focus).

use egui::{Align2, Rect, Sense, Stroke, Ui, Vec2};

use super::{icons, markdown, theme, widgets, UiState};
use crate::agent::{ConfirmOutcome, ConfirmationDetails};

/// Header row: icon, title, optional monospace target and `+N -M` stats on the right.
fn header(ui: &mut Ui, icon: &str, title: &str, target: &str, stats: Option<(usize, usize)>) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 22.0), Sense::hover());
    let p = ui.painter();
    let y = rect.center().y;
    p.text(egui::pos2(rect.left(), y), Align2::LEFT_CENTER, icon, theme::icon(15.0), theme::FG_SECONDARY);
    let tr = p.text(egui::pos2(rect.left() + 24.0, y), Align2::LEFT_CENTER, title, theme::medium(13.5), theme::FG);
    let mut right = rect.right();
    if let Some((a, r)) = stats {
        let rr = p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, format!("-{r}"), theme::mono(12.5), theme::ERROR);
        let ar = p.text(egui::pos2(rr.left() - 6.0, y), Align2::RIGHT_CENTER, format!("+{a}"), theme::mono(12.5), theme::SUCCESS);
        right = ar.left() - 12.0;
    }
    if !target.is_empty() {
        let r = Rect::from_min_max(egui::pos2(tr.right() + 8.0, rect.top()), egui::pos2(right, rect.bottom()));
        widgets::paint_truncated(ui, r, target, theme::mono(12.5), theme::FG_SECONDARY);
    }
}

/// Inset block (bg fill, radius 8) inside the confirmation card.
fn inset<R>(ui: &mut Ui, pad: i8, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(theme::RADIUS_CODE)
        .inner_margin(pad)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn inline(ui: &mut Ui, state: &mut UiState) {
    let Some(pending) = &state.pending_confirm else { return };
    let req = &pending.request;
    let mut outcome = None;
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::PRIMARY.gamma_multiply(0.4)))
        .corner_radius(theme::RADIUS_CARD)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            match &req.details {
                ConfirmationDetails::Edit { path, diff, is_new_file, .. } => {
                    let (icon, title) = if *is_new_file { (icons::FILE_PLUS, "Create file?") } else { (icons::PENCIL_SIMPLE_LINE, "Apply edit?") };
                    header(ui, icon, title, path, Some(markdown::diff_stats(diff)));
                    inset(ui, 0, |ui| {
                        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 6)).show(ui, |ui| markdown::render_diff(ui, &req.call_id, diff));
                    });
                }
                ConfirmationDetails::Shell { command, dir, description, simple, .. } => {
                    // A line that deletes files is never approved automatically (see the policy);
                    // the card says so before the user decides.
                    let deletes = crate::agent::tools::deletes_files(command);
                    if deletes {
                        header(ui, icons::WARNING, "Delete files?", "", None);
                    } else {
                        header(ui, icons::TERMINAL_WINDOW, "Run command?", "", None);
                    }
                    // The command itself comes first; the model's description is secondary.
                    inset(ui, 10, |ui| {
                        ui.horizontal_top(|ui| {
                            ui.label(egui::RichText::new("$").font(theme::mono(12.5)).color(theme::FG_MUTED));
                            ui.add(egui::Label::new(egui::RichText::new(command).font(theme::mono(12.5)).color(theme::FG)).wrap());
                        });
                    });
                    ui.label(egui::RichText::new(format!("in {dir}")).font(theme::font(12.0)).color(theme::FG_MUTED));
                    if deletes {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(14.0)).color(theme::ERROR));
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(
                                        "This command deletes files. AntiCraft cannot bring them back, and it never approves a deletion by itself.",
                                    )
                                    .font(theme::font(12.5))
                                    .color(theme::ERROR),
                                )
                                .wrap(),
                            );
                        });
                    }
                    if !*simple {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(13.0)).color(theme::WARNING));
                            ui.label(
                                egui::RichText::new("Compound command (pipes, ';', subexpressions or redirection).")
                                    .font(theme::font(12.0))
                                    .color(theme::WARNING),
                            );
                        });
                    }
                    if let Some(d) = description {
                        ui.add(
                            egui::Label::new(egui::RichText::new(format!("Model's description (unverified): {d}")).font(theme::font(12.0)).italics().color(theme::FG_MUTED))
                                .wrap(),
                        );
                    }
                }
                ConfirmationDetails::Read { summary } => {
                    header(ui, icons::FILE_TEXT, "Allow read access?", "", None);
                    ui.add(egui::Label::new(egui::RichText::new(summary).font(theme::font(13.0)).color(theme::FG_SECONDARY)).wrap());
                }
                ConfirmationDetails::Delete { path, .. } => {
                    header(ui, icons::WARNING, "Delete file?", path, None);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(14.0)).color(theme::ERROR));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    "The model wants to overwrite this file with empty content. Everything in it will be lost.",
                                )
                                .font(theme::font(12.5))
                                .color(theme::ERROR),
                            )
                            .wrap(),
                        );
                    });
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(
                        egui::RichText::new("This action cannot be undone from within AntiCraft. It is never approved automatically.")
                            .font(theme::font(12.0)).color(theme::FG_MUTED).italics()
                    ).wrap());
                }
                // No plug/cloud glyph exists in the icon set, so the wrench stands in for
                // "some tool that is not ours". The title never claims read-only: that is the
                // server's own description of the tool, shown as a muted note below.
                ConfirmationDetails::Mcp { server, tool, summary, read_only } => {
                    header(ui, icons::WRENCH, "Call MCP tool?", "", None);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(server).font(theme::mono(12.5)).color(theme::FG_MUTED));
                        ui.label(egui::RichText::new("/").font(theme::mono(12.5)).color(theme::FG_MUTED));
                        ui.label(egui::RichText::new(tool).font(theme::mono(12.5)).color(theme::FG));
                    });
                    inset(ui, 10, |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(summary).font(theme::mono(12.0)).color(theme::FG)).wrap());
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(13.0)).color(theme::WARNING));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    "Runs in an external server, outside the workspace. AntiCraft does not review what it does.",
                                )
                                .font(theme::font(12.0))
                                .color(theme::WARNING),
                            )
                            .wrap(),
                        );
                    });
                    if *read_only {
                        ui.label(
                            egui::RichText::new("The server describes this tool as read-only.")
                                .font(theme::font(12.0))
                                .color(theme::FG_MUTED),
                        );
                    }
                }
            }
            ui.label(
                egui::RichText::new(format!("{} wants to run. Approve or reject it from the composer below.", req.tool_name))
                    .font(theme::font(12.0))
                    .color(theme::FG_MUTED),
            );
        });
    if outcome.is_none() && !ui.ctx().text_edit_focused() && !egui::Popup::is_any_open(ui.ctx()) {
        ui.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                outcome = Some(ConfirmOutcome::Cancel);
            } else if i.key_pressed(egui::Key::Enter) {
                outcome = Some(ConfirmOutcome::ProceedOnce);
            }
        });
    }
    if let Some(o) = outcome {
        state.answer_confirm(o);
    }
}

/// 24px tray button: `#2d2d2d` secondary, `#007acc` primary (white text), ghost text-only.
fn tray_button(ui: &mut Ui, label: &str, kind: widgets::ButtonKind, hint: &str) -> bool {
    let font = if kind == widgets::ButtonKind::Primary { theme::medium(12.5) } else { theme::font(12.5) };
    let g = ui.painter().layout_no_wrap(label.to_owned(), font, theme::FG);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x + 24.0, 24.0), Sense::click());
    let hovered = r.hovered();
    let (fill, fg) = match kind {
        widgets::ButtonKind::Primary => (if hovered { theme::PRIMARY.lerp_to_gamma(egui::Color32::WHITE, 0.12) } else { theme::PRIMARY }, egui::Color32::WHITE),
        widgets::ButtonKind::Secondary => (if hovered { theme::PRESSED } else { theme::SEND_BG }, theme::FG),
        widgets::ButtonKind::Ghost => (if hovered { theme::SECONDARY } else { egui::Color32::TRANSPARENT }, if hovered { theme::FG } else { theme::FG_SECONDARY }),
    };
    let p = ui.painter();
    p.rect_filled(rect, 6, fill);
    p.galley_with_override_text_color(rect.center() - g.size() / 2.0, g, fg);
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
    let r = if hint.is_empty() { r } else { r.on_hover_text(hint) };
    r.clicked()
}

/// Inline action row inside the docked composer card while a confirmation is pending:
/// right-aligned, 15px from the card edge, 18px between the main buttons.
pub fn tray_actions(ui: &mut Ui, state: &mut UiState) {
    let Some(pending) = &state.pending_confirm else { return };
    let req = &pending.request;
    let always = match &req.details {
        ConfirmationDetails::Shell { root_command, simple: true, .. } if !root_command.is_empty() => {
            Some((format!("Always allow '{root_command}'"), "Allow this command for the rest of the session"))
        }
        ConfirmationDetails::Shell { .. } => None,
        // Deletions are never session-approved: the user must confirm each one.
        ConfirmationDetails::Delete { .. } => None,
        ConfirmationDetails::Edit { .. }
        | ConfirmationDetails::Read { .. }
        | ConfirmationDetails::Mcp { .. } => {
            Some(("Always allow".to_owned(), "Allow this tool for the rest of the session"))
        }
    };
    let prompt = format!("{} wants to run", req.tool_name);
    let mut outcome = None;
    let w = ui.available_width();
    ui.allocate_ui_with_layout(Vec2::new(w, 28.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.set_min_size(Vec2::new(w, 28.0));
        ui.spacing_mut().item_spacing.x = 18.0;
        // 12px card margin + 1px (plus the 1px stroke and rounding) gives the 15px right inset.
        ui.add_space(1.0);
        if tray_button(ui, "Approve", widgets::ButtonKind::Primary, "Enter") {
            outcome = Some(ConfirmOutcome::ProceedOnce);
        }
        if tray_button(ui, "Reject", widgets::ButtonKind::Secondary, "Esc") {
            outcome = Some(ConfirmOutcome::Cancel);
        }
        ui.spacing_mut().item_spacing.x = 8.0;
        if let Some((label, tip)) = always {
            if tray_button(ui, &label, widgets::ButtonKind::Ghost, tip) {
                outcome = Some(ConfirmOutcome::ProceedAlways);
            }
        }
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(13.0)).color(theme::FG_MUTED));
            ui.label(egui::RichText::new(prompt).font(theme::font(12.0)).color(theme::FG_SECONDARY));
        });
    });
    if let Some(o) = outcome {
        state.answer_confirm(o);
    }
}
