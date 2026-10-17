//! Compact editor search controls. SQL changes and cursor movement are applied
//! by the console after this view has collected the user's intent.

use crate::search::SearchMatch;
use crate::ui::theme;
use egui::{Id, Key, Modifiers, RichText, Ui};

pub(in crate::ui) struct SearchState {
    pub(super) visible: bool,
    pub(super) replace_mode: bool,
    pub(super) query: String,
    pub(super) replacement: String,
    pub(super) match_case: bool,
    pub(super) whole_words: bool,
    pub(super) focus_query: bool,
    pub(super) matches: Vec<SearchMatch>,
    pub(super) active: Option<usize>,
    pub(super) dirty: bool,
    pub(super) highlights: Vec<std::ops::Range<usize>>,
    pub(super) projected_folds: Vec<std::ops::Range<usize>>,
    pub(super) projection_dirty: bool,
    pub(super) goto_open: bool,
    pub(super) goto_text: String,
    pub(super) goto_error: Option<String>,
    goto_was_open: bool,
    focusable_buttons: Vec<Id>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            visible: false,
            replace_mode: false,
            query: String::new(),
            replacement: String::new(),
            match_case: false,
            whole_words: false,
            focus_query: false,
            matches: Vec::new(),
            active: None,
            dirty: true,
            highlights: Vec::new(),
            projected_folds: Vec::new(),
            projection_dirty: true,
            goto_open: false,
            goto_text: String::new(),
            goto_error: None,
            goto_was_open: false,
            focusable_buttons: Vec::with_capacity(8),
        }
    }
}

#[derive(Default)]
pub(super) struct SearchAction {
    pub(super) query_changed: bool,
    pub(super) next: bool,
    pub(super) previous: bool,
    pub(super) replace: bool,
    pub(super) replace_all: bool,
    pub(super) close: bool,
    pub(super) return_editor_focus: bool,
    pub(super) goto_submit: Option<String>,
}

impl SearchState {
    /// Include keyboard focus on toolbar actions as well as the text fields.
    /// IDs from the previous frame let console shortcuts run before this view.
    pub(super) fn owns_focus(&self, ui: &Ui, tab_id: u64) -> bool {
        self.action_has_focus(ui.ctx(), tab_id)
    }

    pub(super) fn action_has_focus(&self, ctx: &egui::Context, tab_id: u64) -> bool {
        self.visible
            && ctx.memory(|memory| {
                let owns = |id| {
                    memory.has_focus(id)
                        || (memory.focused().is_none() && memory.had_focus_last_frame(id))
                };
                owns(Id::new(("console_find", tab_id)))
                    || (self.replace_mode && owns(Id::new(("console_replace", tab_id))))
                    || self.focusable_buttons.iter().copied().any(owns)
            })
    }

    pub(super) fn show(&mut self, ui: &mut Ui, tab_id: u64, read_only: bool) -> SearchAction {
        let mut action = SearchAction::default();
        if self.visible {
            self.find_bar(ui, tab_id, read_only, &mut action);
        }
        self.go_to_window(ui, tab_id, &mut action);
        action
    }

    fn find_bar(&mut self, ui: &mut Ui, tab_id: u64, read_only: bool, action: &mut SearchAction) {
        self.focusable_buttons.clear();
        let query_id = Id::new(("console_find", tab_id));
        let replacement_id = Id::new(("console_replace", tab_id));
        let (query_focused, replacement_focused) = ui.memory(|memory| {
            (
                memory.has_focus(query_id),
                self.replace_mode && memory.has_focus(replacement_id),
            )
        });
        // Consume Enter before TextEdit handles it and surrenders focus. Search
        // navigation should leave the field ready for another query.
        if query_focused || replacement_focused {
            ui.input_mut(|input| {
                if input.consume_key(Modifiers::SHIFT, Key::Enter) {
                    action.previous = true;
                } else if input.consume_key(Modifiers::NONE, Key::Enter) {
                    if replacement_focused {
                        action.replace = !self.matches.is_empty();
                    } else {
                        action.next = true;
                    }
                }
                action.close = input.consume_key(Modifiers::NONE, Key::Escape);
            });
        }

        let mut bar_focused = query_focused || replacement_focused;
        egui::Frame::new()
            .fill(theme::BG_PANEL)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let field_width = (ui.available_width() - 280.0).max(80.0);
                ui.horizontal(|ui| {
                    let toggle = icon_button(
                        ui,
                        if self.replace_mode { "⌄" } else { "›" },
                        "Show Replace",
                        &theme::keys("Replace  ⌘R"),
                        !read_only,
                        self.replace_mode,
                    );
                    self.focusable_buttons.push(toggle.id);
                    bar_focused |= toggle.has_focus();
                    if toggle.clicked() {
                        self.replace_mode = !self.replace_mode;
                    }
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .id(query_id)
                            .desired_width(field_width)
                            .hint_text("Find"),
                    );
                    field.widget_info(|| {
                        let mut info =
                            egui::WidgetInfo::text_edit(true, &self.query, &self.query, "");
                        info.label = Some("Find".into());
                        info
                    });
                    let field = field.on_hover_text(theme::keys("Find  ⌘F"));
                    if self.focus_query {
                        focus_and_select_all(ui, &field, &self.query);
                        self.focus_query = false;
                    }
                    action.query_changed |= field.changed();
                    bar_focused |= field.has_focus();
                    for (symbol, label, selected) in [
                        ("Cc", "Match Case", &mut self.match_case),
                        ("W", "Words", &mut self.whole_words),
                    ] {
                        let button = icon_button(ui, symbol, label, label, true, *selected);
                        self.focusable_buttons.push(button.id);
                        bar_focused |= button.has_focus();
                        if button.clicked() {
                            *selected = !*selected;
                            action.query_changed = true;
                        }
                    }
                    let count = if self.query.is_empty() {
                        String::new()
                    } else if self.matches.is_empty() {
                        "No matches".into()
                    } else if let Some(active) = self.active.filter(|i| *i < self.matches.len()) {
                        format!("{} of {}", active + 1, self.matches.len())
                    } else {
                        format!("{} matches", self.matches.len())
                    };
                    let count_color = if !self.query.is_empty() && self.matches.is_empty() {
                        theme::RED
                    } else {
                        theme::TEXT_MUTED
                    };
                    ui.add_sized(
                        [82.0, 24.0],
                        egui::Label::new(RichText::new(count).small().color(count_color)),
                    );
                    for (symbol, label, tooltip, request) in [
                        (
                            "↑",
                            "Previous Occurrence",
                            &*theme::keys("Previous Occurrence  ⌘⇧G"),
                            &mut action.previous,
                        ),
                        (
                            "↓",
                            "Next Occurrence",
                            &*theme::keys("Next Occurrence  ⌘G"),
                            &mut action.next,
                        ),
                    ] {
                        let button = icon_button(
                            ui,
                            symbol,
                            label,
                            tooltip,
                            !self.matches.is_empty(),
                            false,
                        );
                        self.focusable_buttons.push(button.id);
                        bar_focused |= button.has_focus();
                        *request |= button.clicked();
                    }
                    let close = icon_button(ui, "×", "Close Find", "Close Find  Esc", true, false);
                    self.focusable_buttons.push(close.id);
                    bar_focused |= close.has_focus();
                    action.close |= close.clicked();
                });
                if self.replace_mode {
                    ui.horizontal(|ui| {
                        ui.add_space(30.0);
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut self.replacement)
                                .id(replacement_id)
                                .desired_width(field_width)
                                .hint_text("Replace with"),
                        );
                        field.widget_info(|| {
                            let mut info = egui::WidgetInfo::text_edit(
                                true,
                                &self.replacement,
                                &self.replacement,
                                "",
                            );
                            info.label = Some("Replace with".into());
                            info
                        });
                        bar_focused |= field.has_focus();
                        let can_replace = !self.matches.is_empty();
                        let replace = ui.add_enabled(can_replace, egui::Button::new("Replace"));
                        self.focusable_buttons.push(replace.id);
                        action.replace |= replace.clicked();
                        bar_focused |= replace.has_focus();
                        let replace_all =
                            ui.add_enabled(can_replace, egui::Button::new("Replace All"));
                        self.focusable_buttons.push(replace_all.id);
                        action.replace_all |= replace_all.clicked();
                        bar_focused |= replace_all.has_focus();
                    });
                }
            });
        if bar_focused {
            action.close |= ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        }
        if action.close {
            self.visible = false;
            action.return_editor_focus = true;
        }
        if action.query_changed {
            self.dirty = true;
        }
        theme::hairline(ui);
    }

    fn go_to_window(&mut self, ui: &Ui, tab_id: u64, action: &mut SearchAction) {
        if !self.goto_open {
            self.goto_was_open = false;
            return;
        }
        let initial_focus = !self.goto_was_open;
        self.goto_was_open = true;
        let mut open = true;
        let mut cancel = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        let input_id = Id::new(("console_goto", tab_id));
        let mut submit = ui.memory(|memory| memory.has_focus(input_id))
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
        theme::show_window(ui.ctx(), "Go to Line/Column", &mut open, |window| window
            .id(Id::new(("console_goto_window", tab_id)))
            
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(300.0)
            .default_height(100.0), |ui| {
                let label = ui.label("Line:Column");
                let field = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.goto_text)
                            .id(input_id)
                            .desired_width(f32::INFINITY)
                            .hint_text("Line or line:column"),
                    )
                    .labelled_by(label.id);
                if initial_focus {
                    focus_and_select_all(ui, &field, &self.goto_text);
                }
                if field.changed() {
                    self.goto_error = None;
                }
                if let Some(error) = &self.goto_error {
                    ui.label(RichText::new(error).color(theme::RED));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        submit |= theme::primary_button(ui, "Go").clicked();
                        cancel |= theme::dialog_button(ui, "Cancel").clicked();
                    });
                });
            });
        if cancel || !open {
            self.goto_open = false;
            self.goto_was_open = false;
            self.goto_error = None;
            action.return_editor_focus = true;
        } else if submit {
            // The parent closes the window only after validating the position.
            // Invalid input should return focus to the field; a successful
            // submission must also allow focus when the popup is reopened.
            self.goto_was_open = false;
            action.goto_submit = Some(self.goto_text.clone());
        }
    }
}

fn focus_and_select_all(ui: &Ui, response: &egui::Response, text: &str) {
    let mut state = egui::TextEdit::load_state(ui.ctx(), response.id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(text.chars().count()),
        )));
    state.store(ui.ctx(), response.id);
    response.request_focus();
    ui.ctx().request_repaint();
}

fn icon_button(
    ui: &mut Ui,
    symbol: &str,
    label: &str,
    tooltip: &str,
    enabled: bool,
    selected: bool,
) -> egui::Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(symbol)
            .frame_when_inactive(false)
            .selected(selected)
            .stroke(egui::Stroke::NONE)
            .min_size(egui::vec2(26.0, 24.0)),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, label)
    });
    response.on_hover_text(tooltip)
}
