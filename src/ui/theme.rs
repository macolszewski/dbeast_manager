//! Dark look: palette, fonts and the small widgets shared
//! by tool windows (flat toolbar buttons, underlined tabs, headers).
//!
//! The interface and SQL syntax colors are taken from the dark theme of the
//! IntelliJ Platform (Apache License 2.0); the attribution is in NOTICE.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response,
    RichText, Sense, Stroke, TextStyle, Ui, Visuals,
};
use std::sync::Arc;

pub const BG_EDITOR: Color32 = Color32::from_rgb(0x1E, 0x1F, 0x22);
pub const BG_PANEL: Color32 = Color32::from_rgb(0x2B, 0x2D, 0x30);
pub const BG_HOVER: Color32 = Color32::from_rgb(0x39, 0x3B, 0x40);
pub const BG_SELECTED: Color32 = Color32::from_rgb(0x2E, 0x43, 0x6E);
pub const BG_CURRENT_LINE: Color32 = Color32::from_rgb(0x26, 0x28, 0x2E);
pub const BG_INPUT: Color32 = Color32::from_rgb(0x2B, 0x2D, 0x30);
pub const BORDER: Color32 = Color32::from_rgb(0x1E, 0x1F, 0x22);
pub const BORDER_INPUT: Color32 = Color32::from_rgb(0x4E, 0x51, 0x57);
pub const GRID_LINE: Color32 = Color32::from_rgb(0x31, 0x33, 0x37);
pub const TEXT: Color32 = Color32::from_rgb(0xDF, 0xE1, 0xE5);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x86, 0x8A, 0x91);
pub const LINE_NUMBER: Color32 = Color32::from_rgb(0x4B, 0x50, 0x59);
pub const ACCENT: Color32 = Color32::from_rgb(0x35, 0x74, 0xF0);
pub const GREEN: Color32 = Color32::from_rgb(0x5F, 0xB8, 0x65);
pub const RED: Color32 = Color32::from_rgb(0xDB, 0x5C, 0x5C);

pub const SQL_KEYWORD: Color32 = Color32::from_rgb(0xCF, 0x8E, 0x6D);
pub const SQL_STRING: Color32 = Color32::from_rgb(0x6A, 0xAB, 0x73);
pub const SQL_NUMBER: Color32 = Color32::from_rgb(0x2A, 0xAC, 0xB8);
pub const SQL_COMMENT: Color32 = Color32::from_rgb(0x7A, 0x7E, 0x85);
pub const SQL_IDENT: Color32 = Color32::from_rgb(0xC7, 0x7D, 0xBB);
pub const SQL_FUNCTION: Color32 = Color32::from_rgb(0x56, 0xA8, 0xF5);
pub const SEARCH_MATCH: Color32 = Color32::from_rgb(92, 75, 30);
pub const SEARCH_CURRENT_MATCH: Color32 = BG_SELECTED;
pub const SEARCH_MARKER: Color32 = Color32::from_rgb(0xD9, 0xA3, 0x43);
/// Background of a switched-on toggle or the open tool window;
/// neutral, so it is not mistaken for a selection.
pub const BG_TOGGLE_ON: Color32 = Color32::from_rgb(0x43, 0x45, 0x4A);
/// Icons on panels: lighter than muted text so they do not fade out.
pub const ICON: Color32 = Color32::from_rgb(0xA8, 0xAD, 0xB5);
/// Alternate row background on the editor background (grids, Record View).
pub const STRIPE: Color32 = Color32::from_rgb(0x29, 0x2B, 0x2F);
/// Alternate row background on panels and dialogs, which are lighter.
pub const STRIPE_ON_PANEL: Color32 = Color32::from_rgb(0x34, 0x36, 0x3A);

/// Height of tool window headers and the editor tab row.
pub const HEADER_HEIGHT: f32 = 32.0;

/// Text style of result grid cells; its size follows the Appearance settings.
pub fn grid_text_style() -> TextStyle {
    static STYLE: std::sync::LazyLock<TextStyle> =
        std::sync::LazyLock::new(|| TextStyle::Name("grid".into()));
    STYLE.clone()
}

/// Font of result grid cells; the default size before the theme is applied.
pub fn grid_font(ui: &Ui) -> FontId {
    ui.style()
        .text_styles
        .get(&grid_text_style())
        .cloned()
        .unwrap_or_else(|| mono(13.0))
}

fn appearance_id() -> egui::Id {
    egui::Id::new("dbeast_appearance")
}

/// Makes the Appearance settings current: the grid font size goes into the
/// style, the rest is read by the editor and the grid when they draw.
pub fn set_appearance(ctx: &egui::Context, appearance: &crate::config::Appearance) {
    let appearance = appearance.clone().clamped();
    let grid_font = mono(appearance.grid_font_size);
    ctx.global_style_mut(|style| {
        style.text_styles.insert(grid_text_style(), grid_font);
    });
    ctx.data_mut(|d| d.insert_temp(appearance_id(), appearance));
}

pub fn appearance(ctx: &egui::Context) -> crate::config::Appearance {
    ctx.data(|d| d.get_temp(appearance_id())).unwrap_or_default()
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Inter SemiBold, used for tool window titles and dialog headings.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        &mut fonts,
        "inter",
        include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
    );
    add(
        &mut fonts,
        "inter-semibold",
        include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
    );
    add(
        &mut fonts,
        "jetbrains-mono",
        include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"),
    );

    // egui's bundled fonts stay as fallbacks for symbols and emoji icons.
    let fallbacks = fonts.families[&FontFamily::Proportional].clone();
    fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .unwrap()
        .insert(0, "inter".into());
    fonts
        .families
        .get_mut(&FontFamily::Monospace)
        .unwrap()
        .insert(0, "jetbrains-mono".into());
    let mut semibold = vec!["inter-semibold".to_string()];
    semibold.extend(fallbacks);
    fonts
        .families
        .insert(FontFamily::Name("semibold".into()), semibold);
    fonts
}

pub fn apply(ctx: &egui::Context) {
    // ⌘= / ⌘- fold code; the interface zooms with Zoom IDE instead.
    ctx.options_mut(|options| options.zoom_with_keyboard = false);
    ctx.set_fonts(fonts());

    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG_PANEL;
    visuals.window_fill = BG_PANEL;
    visuals.extreme_bg_color = BG_EDITOR;
    visuals.faint_bg_color = STRIPE;
    visuals.code_bg_color = BG_EDITOR;
    // Fields are darker than panels, so forms do not blend in.
    visuals.text_edit_bg_color = Some(BG_EDITOR);
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = BG_SELECTED;
    visuals.selection.stroke = Stroke::new(1.5, ACCENT);
    visuals.hyperlink_color = Color32::from_rgb(0x54, 0x8A, 0xF7);
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x43, 0x45, 0x4A));
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    // Inputs and dialog buttons have a visible border; toolbar buttons opt out via `tool_button`.
    visuals.widgets.inactive.bg_fill = BG_INPUT;
    visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_INPUT);
    visuals.widgets.hovered.weak_bg_fill = BG_HOVER;
    visuals.widgets.hovered.bg_fill = BG_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x5A, 0x5D, 0x63));
    visuals.widgets.active.weak_bg_fill = BG_SELECTED;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.open.weak_bg_fill = BG_HOVER;
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(4);
    }
    // Always dark, regardless of the OS appearance.
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, visuals);

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.spacing.interact_size.y = 24.0;
        style.spacing.menu_margin = Margin::same(6);
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(13.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(13.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(11.5));
        style.text_styles.insert(TextStyle::Heading, semibold(15.0));
        style.text_styles.insert(TextStyle::Monospace, mono(13.0));
    });
    set_appearance(ctx, &crate::config::Appearance::default());
}

/// Flat icon/text button as in IDE toolbars: no border, background on hover only.
pub fn tool_button(ui: &mut Ui, text: impl Into<RichText>, tooltip: &str) -> Response {
    let btn = egui::Button::new(text.into())
        .frame_when_inactive(false)
        .stroke(Stroke::NONE)
        .min_size(egui::vec2(26.0, 24.0));
    ui.add(btn).on_hover_text(tooltip)
}

/// Same as [`tool_button`], but greyed out and inert when `enabled` is false.
pub fn tool_button_enabled(
    ui: &mut Ui,
    enabled: bool,
    text: impl Into<RichText>,
    tooltip: &str,
) -> Response {
    ui.add_enabled_ui(enabled, |ui| tool_button(ui, text, tooltip))
        .inner
}

/// Flat drop-down button for toolbars (no border until hovered).
pub fn tool_menu<R>(
    ui: &mut Ui,
    text: impl Into<egui::WidgetText>,
    tooltip: &str,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> Response {
    ui.scope(|ui| {
        flat_widgets(ui);
        ui.menu_button(text, add_contents)
            .response
            .on_hover_text(tooltip)
    })
    .inner
}

/// Removes borders from inactive widgets in this scope, e.g. combo boxes in a toolbar.
pub fn flat_widgets(ui: &mut Ui) {
    let w = &mut ui.visuals_mut().widgets;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_fill = Color32::TRANSPARENT;
    w.hovered.bg_stroke = Stroke::NONE;
    // A switched-on toolbar toggle is neutral grey, not the
    // selection blue used by grid cells and tree rows.
    let selection = &mut ui.visuals_mut().selection;
    selection.bg_fill = BG_TOGGLE_ON;
    selection.stroke = Stroke::new(1.0, TEXT);
}

/// Filled blue button for the default action of a dialog.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE))
            .fill(ACCENT)
            .stroke(Stroke::NONE)
            .min_size(egui::vec2(72.0, 26.0)),
    )
}

pub fn dialog_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(egui::vec2(72.0, 26.0)))
}

/// Tab with the active one underlined in accent blue, like editor and tool window tabs.
/// Returns the tab response and whether its close button was clicked.
pub fn flat_tab(ui: &mut Ui, label: RichText, active: bool, closable: bool) -> (Response, bool) {
    let mut closed = false;
    let text_color = if active { TEXT } else { TEXT_MUTED };
    let galley = egui::WidgetText::from(label.color(text_color)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Body,
    );
    let close_w = if closable { 18.0 } else { 0.0 };
    let size = egui::vec2(galley.size().x + 20.0 + close_w, HEADER_HEIGHT - 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            true,
            active,
            galley.text(),
        )
    });
    let painter = ui.painter_at(rect);
    if resp.hovered() && !active {
        painter.rect_filled(rect.shrink2(egui::vec2(0.0, 3.0)), 4.0, BG_HOVER);
    }
    let text_pos = egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, text_color);
    if active {
        let underline = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 2.0, rect.bottom() - 2.0),
            egui::pos2(rect.right() - 2.0, rect.bottom()),
        );
        painter.rect_filled(underline, 1.0, ACCENT);
    }
    if closable {
        let c = egui::pos2(rect.right() - 13.0, rect.center().y);
        let close_rect = egui::Rect::from_center_size(c, egui::vec2(16.0, 16.0));
        let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());
        if close.hovered() {
            painter.rect_filled(close_rect, 3.0, BG_HOVER);
        }
        let color = if close.hovered() { TEXT } else { TEXT_MUTED };
        let d = 3.5;
        let stroke = Stroke::new(1.2, color);
        painter.line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], stroke);
        painter.line_segment([c + egui::vec2(-d, d), c + egui::vec2(d, -d)], stroke);
        closed = close.clicked();
    }
    (resp, closed)
}

/// Title row of a tool window: bold title on the left, actions on the right.
/// The actions are laid out first, so on a narrow panel the title is shortened
/// with an ellipsis instead of being drawn under the buttons.
pub fn tool_window_header(ui: &mut Ui, title: &str, actions: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), HEADER_HEIGHT),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            actions(ui);
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(RichText::new(title).font(semibold(13.0)))
                        .truncate()
                        .selectable(false),
                )
                .on_hover_text(title);
            });
        },
    );
}

/// 1 px separator line in the panel border color, drawn across the available width.
/// Section title with a hairline to the right edge, as in forms.
pub fn section_header(ui: &mut Ui, text: &str) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).font(semibold(13.0)));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().rect_filled(rect.translate(egui::vec2(6.0, 1.0)), 0.0, GRID_LINE);
    });
    ui.add_space(6.0);
}

/// A small ⓘ with the explanation in its tooltip, instead of a paragraph.
pub fn help_icon(ui: &mut Ui, text: &str) -> Response {
    // Painted, not a glyph: the bundled fonts have no ⓘ.
    let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
    let color = if response.hovered() { TEXT } else { TEXT_MUTED };
    let center = rect.center();
    ui.painter().circle_stroke(center, 6.5, Stroke::new(1.2, color));
    ui.painter().text(center + egui::vec2(0.0, 0.5), egui::Align2::CENTER_CENTER, "i", semibold(10.0), color);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Help"));
    response.on_hover_text(text)
}

/// Dialog footer: a hairline, extra actions on the left and
/// the buttons right-aligned, primary first from the right edge inwards.
pub fn dialog_footer(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    ui.add_space(8.0);
    hairline(ui);
    ui.add_space(8.0);
    // A fixed-height row: a right-to-left layout would otherwise take all the
    // height left in the dialog and grow it.
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 28.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            left(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
        },
    );
}

/// Form row: a fixed-width label column, then the field, as in IDE settings.
pub fn form_row(ui: &mut Ui, label: &str, label_width: f32, field: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(label_width, 24.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(label_width);
                ui.label(label);
            },
        );
        field(ui);
    });
    ui.add_space(4.0);
}

/// Frame of a tool panel docked beside the grid (Record View, Aggregates):
/// the panel background sets it apart from the editor-coloured grid.
pub fn side_panel_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_PANEL)
        .inner_margin(egui::Margin::symmetric(8, 0))
}

/// Fixed 28 px title row of a docked tool panel, followed by a hairline.
/// Table stripes below it use the stripe colour meant for the panel background.
pub fn panel_header(ui: &mut Ui, contents: impl FnOnce(&mut Ui)) {
    ui.visuals_mut().faint_bg_color = STRIPE_ON_PANEL;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 28.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(ui.available_width(), 28.0));
            flat_widgets(ui);
            contents(ui);
        },
    );
    hairline(ui);
    ui.add_space(4.0);
}

/// Muted text centred in an empty view, such as "Nothing to show".
pub fn empty_text(ui: &mut Ui, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new(text).color(TEXT_MUTED));
    });
}

pub fn hairline(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, BORDER);
}

/// Shortcut text written with macOS symbols (⌘⇧L, ⌥⌘L, ⌘↵), shown as is on
/// macOS and as Ctrl+Shift+L, Ctrl+Alt+L, Ctrl+Enter elsewhere, where the
/// same `Modifiers::COMMAND` shortcuts use Ctrl.
pub fn keys(text: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(target_os = "macos") {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(keys_for_other_systems(text))
    }
}

fn keys_for_other_systems(text: &str) -> String {
    let modifier = |c: char| matches!(c, '⌃' | '⌥' | '⇧' | '⌘');
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if !modifier(c) {
            out.push(c);
            continue;
        }
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        let mut current = c;
        loop {
            match current {
                '⌘' | '⌃' => ctrl = true,
                '⌥' => alt = true,
                _ => shift = true,
            }
            match chars.next_if(|n| modifier(*n)) {
                Some(next) => current = next,
                None => break,
            }
        }
        for (on, name) in [(ctrl, "Ctrl+"), (alt, "Alt+"), (shift, "Shift+")] {
            if on {
                out.push_str(name);
            }
        }
        // The key: a function key such as F2, or one character.
        match chars.next() {
            Some('↵') => out.push_str("Enter"),
            Some('⌫') => out.push_str("Backspace"),
            Some('F') if chars.peek().is_some_and(char::is_ascii_digit) => {
                out.push('F');
                while let Some(d) = chars.next_if(char::is_ascii_digit) {
                    out.push(d);
                }
            }
            Some(key) => out.push(key),
            None => {}
        }
    }
    out
}


/// A modal dialog: the rest of the application waits behind a
/// dimmed backdrop, while the dialog itself can be moved and, unless it is a
/// short confirmation, resized. It opens centred; Escape or the title bar's
/// close button close it, a click beside it does not.
pub struct Dialog {
    id: egui::Id,
    title: egui::WidgetText,
    resizable: bool,
    default_size: egui::Vec2,
    min_size: egui::Vec2,
    frame: Option<egui::Frame>,
}

pub struct DialogResponse {
    pub is_top_modal: bool,
    pub any_popup_open: bool,
    closed: bool,
    escape: bool,
}

impl DialogResponse {
    pub fn should_close(&self) -> bool {
        self.closed || (self.is_top_modal && !self.any_popup_open && self.escape)
    }
}

impl Dialog {
    pub fn new(id: egui::Id, title: impl Into<egui::WidgetText>) -> Self {
        Self {
            id,
            title: title.into(),
            resizable: true,
            default_size: egui::vec2(480.0, 320.0),
            min_size: egui::vec2(260.0, 120.0),
            frame: None,
        }
    }

    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Initial size, kept within the window; the user may change it.
    pub fn default_size(mut self, size: impl Into<egui::Vec2>) -> Self {
        self.default_size = size.into();
        self
    }

    pub fn min_size(mut self, size: impl Into<egui::Vec2>) -> Self {
        self.min_size = size.into();
        self
    }

    pub fn frame(mut self, frame: egui::Frame) -> Self {
        self.frame = Some(frame);
        self
    }

    pub fn show<T>(self, ctx: &egui::Context, content: impl FnOnce(&mut Ui) -> T) -> DialogResponse {
        let screen = ctx.content_rect();
        let window_layer = egui::LayerId::new(egui::Order::Foreground, self.id);
        let is_top_modal = ctx.memory_mut(|memory| {
            memory.set_modal_layer(window_layer);
            memory.top_modal_layer() == Some(window_layer)
        });
        let any_popup_open = egui::Popup::is_any_open(ctx);
        // egui's modal backdrop, without content, dims and shields everything
        // behind the dialog; the window above it becomes the modal layer.
        egui::Modal::new(self.id.with("backdrop"))
            .backdrop_color(Color32::from_black_alpha(90))
            .frame(egui::Frame::NONE)
            .show(ctx, |_| {});
        ctx.memory_mut(|memory| memory.set_modal_layer(window_layer));
        let size = self
            .default_size
            .min(screen.size() - egui::vec2(32.0, 48.0))
            .max(self.min_size);
        let mut open = true;
        let frame = self.frame.unwrap_or_else(|| egui::Frame::window(&ctx.global_style()));
        show_window(
            ctx,
            self.title,
            &mut open,
            |window| {
                window
                    .id(self.id)
                    .order(egui::Order::Foreground)
                    .collapsible(false)
                    .movable(true)
                    .resizable(self.resizable)
                    .pivot(egui::Align2::CENTER_CENTER)
                    .default_pos(screen.center())
                    .default_size(size)
                    .min_size(self.min_size.min(size))
                    .max_size(screen.size() - egui::vec2(16.0, 16.0))
                    .constrain(true)
                    .frame(frame)
            },
            |ui| {
                // Dialogs are panel-coloured; striped rows need a lighter stripe there.
                ui.visuals_mut().faint_bg_color = STRIPE_ON_PANEL;
                content(ui)
            },
        );
        // Above its own backdrop when it opens; later popups (combo boxes,
        // menus) must stay above it, so it is not raised on every frame.
        if !is_top_modal {
            ctx.move_to_top(window_layer);
        }
        DialogResponse {
            is_top_modal,
            any_popup_open,
            closed: !open,
            escape: ctx.input(|i| i.key_pressed(egui::Key::Escape)),
        }
    }
}

/// A closable window with the close control where the platform puts it:
/// on macOS a title bar with the traffic lights on the left and the title
/// centred, elsewhere egui's title bar with × on the right.
pub fn show_window<'w, R>(
    ctx: &egui::Context,
    title: impl Into<egui::WidgetText>,
    open: &'w mut bool,
    configure: impl FnOnce(egui::Window<'w>) -> egui::Window<'w>,
    content: impl FnOnce(&mut Ui) -> R,
) -> Option<egui::InnerResponse<Option<R>>> {
    #[cfg(target_os = "macos")]
    {
        let title = title.into();
        configure(egui::Window::new(title.clone()).title_bar(false)).show(ctx, |ui| {
            traffic_lights_title_bar(ui, title, open);
            content(ui)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        configure(egui::Window::new(title).open(open)).show(ctx, content)
    }
}

/// Close, minimise and zoom lights; only close applies to an in-app window,
/// so the other two stay grey as on an inactive macOS window.
#[cfg(target_os = "macos")]
fn traffic_lights_title_bar(ui: &mut Ui, title: egui::WidgetText, open: &mut bool) {
    const RADIUS: f32 = 6.0;
    const PITCH: f32 = 20.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), Sense::hover());
    let center = |n: f32| egui::pos2(rect.left() + RADIUS + 1.0 + n * PITCH, rect.center().y);
    let close_rect = egui::Rect::from_center_size(center(0.0), egui::Vec2::splat(RADIUS * 2.0 + 4.0));
    let close = ui.interact(close_rect, ui.id().with("traffic_light_close"), Sense::click());
    close.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close window"));
    let painter = ui.painter();
    painter.circle_filled(center(0.0), RADIUS, Color32::from_rgb(0xFF, 0x5F, 0x57));
    for n in [1.0, 2.0] {
        painter.circle_filled(center(n), RADIUS, Color32::from_gray(0x4D));
    }
    if close.hovered() {
        let stroke = egui::Stroke::new(1.3, Color32::from_rgb(0x4D, 0x00, 0x00));
        let c = center(0.0);
        let d = RADIUS * 0.45;
        painter.line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], stroke);
        painter.line_segment([c + egui::vec2(-d, d), c + egui::vec2(d, -d)], stroke);
    }
    if close.clicked() {
        *open = false;
    }
    // Painted, not a label: the window already carries the title for accessibility.
    let width = (rect.width() - 8.0 * PITCH).max(40.0);
    let galley = title.into_galley(ui, Some(egui::TextWrapMode::Truncate), width, egui::TextStyle::Heading);
    let text_color = ui.visuals().text_color();
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, text_color);
    hairline(ui);
}

#[cfg(test)]
mod key_tests {
    #[test]
    fn mac_symbols_become_ctrl_alt_shift_names() {
        let convert = super::keys_for_other_systems;
        assert_eq!(convert("Open a query console: ⌘⇧L or QL"), "Open a query console: Ctrl+Shift+L or QL");
        assert_eq!(convert("Reformat code (⌥⌘L)"), "Reformat code (Ctrl+Alt+L)");
        assert_eq!(convert("Execute (⌘↵)"), "Execute (Ctrl+Enter)");
        assert_eq!(convert("Cancel (⌘F2)"), "Cancel (Ctrl+F2)");
        assert_eq!(convert("Previous  ⌘⇧G"), "Previous  Ctrl+Shift+G");
        assert_eq!(convert("no keys here"), "no keys here");
    }
}

