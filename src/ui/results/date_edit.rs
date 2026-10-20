//! Calendar selection complements direct editing of the database's date text.

use super::super::theme;
use chrono::{Datelike, Months, NaiveDate};
use egui::{Response, Ui};

fn has_date(type_name: &str) -> bool {
    let name = type_name.to_ascii_lowercase();
    name == "date" || name.starts_with("timestamp") || name.starts_with("datetime")
}

fn has_time(type_name: &str) -> bool {
    let name = type_name.to_ascii_lowercase();
    name.starts_with("timestamp")
        || name.starts_with("datetime")
        || name == "time"
        || name == "timetz"
        || name.starts_with("time(")
        || name.starts_with("time ")
}

#[derive(Debug, PartialEq)]
struct TimeParts {
    prefix: String,
    hour: String,
    minute: String,
    second: String,
    fraction: String,
    zone: String,
}

impl TimeParts {
    fn parse(text: &str, dated: bool) -> Option<Self> {
        let (prefix, time) = if dated {
            date_prefix(text)?;
            let separator = text.get(10..11)?;
            if separator != " " && separator != "T" {
                return None;
            }
            (text[..11].to_owned(), &text[11..])
        } else {
            (String::new(), text)
        };
        let zone_start = time
            .char_indices()
            .find(|(i, c)| *i > 0 && matches!(c, '+' | '-' | 'Z'))
            .map(|(i, _)| i)
            .unwrap_or(time.len());
        let (time, zone) = time.split_at(zone_start);
        let mut pieces = time.splitn(3, ':');
        let hour = pieces.next()?.to_owned();
        let minute = pieces.next()?.to_owned();
        let seconds = pieces.next()?;
        let (second, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
        Some(Self {
            prefix,
            hour,
            minute,
            second: second.into(),
            fraction: fraction.into(),
            zone: zone.into(),
        })
    }

    fn value(&self) -> String {
        format!(
            "{}{}:{}:{}{}{}",
            self.prefix,
            self.hour,
            self.minute,
            self.second,
            if self.fraction.is_empty() {
                String::new()
            } else {
                format!(".{}", self.fraction)
            },
            self.zone
        )
    }
}

fn time_fields(ui: &mut Ui, text: &mut String, type_name: &str, dated: bool) {
    let initial = if text.is_empty() {
        if dated {
            format!("{} 00:00:00", chrono::Local::now().date_naive())
        } else {
            "00:00:00".into()
        }
    } else {
        text.clone()
    };
    let Some(mut parts) = TimeParts::parse(&initial, dated) else {
        ui.label(
            egui::RichText::new("Enter a time as HH:MM:SS to edit its components.").size(11.0),
        );
        return;
    };
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        for (label, value) in [
            ("Hours", &mut parts.hour),
            ("Minutes", &mut parts.minute),
            ("Seconds", &mut parts.second),
        ] {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(label)
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
                let response = ui.add(
                    egui::TextEdit::singleline(value)
                        .font(theme::mono(12.0))
                        .desired_width(80.0),
                );
                ui.ctx()
                    .accesskit_node_builder(response.id, |node| node.set_label(label));
                changed |= response.changed();
            });
        }
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new("Fractional seconds")
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
            let response = ui.add(
                egui::TextEdit::singleline(&mut parts.fraction)
                    .font(theme::mono(12.0))
                    .desired_width(124.0)
                    .hint_text("0"),
            );
            ui.ctx()
                .accesskit_node_builder(response.id, |node| node.set_label("Fractional seconds"));
            changed |= response.changed();
        });
        let name = type_name.to_ascii_lowercase();
        if !parts.zone.is_empty() || name.contains("tz") || name.contains("with time zone") {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("Time zone")
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
                let response = ui.add(
                    egui::TextEdit::singleline(&mut parts.zone)
                        .font(theme::mono(12.0))
                        .desired_width(124.0)
                        .hint_text("+00:00"),
                );
                ui.ctx()
                    .accesskit_node_builder(response.id, |node| node.set_label("Time zone"));
                changed |= response.changed();
            });
        }
    });
    if changed {
        *text = parts.value();
    }
}

pub(super) fn column_chars(type_name: &str) -> usize {
    if type_name.eq_ignore_ascii_case("date") {
        14
    } else if has_date(type_name) {
        38
    } else {
        0
    }
}

fn date_prefix(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()
}

fn replace_date(text: &mut String, date: NaiveDate, type_name: &str) {
    let suffix = if date_prefix(text).is_some() {
        text[10..].to_owned()
    } else if text.is_empty() && !type_name.eq_ignore_ascii_case("date") {
        " 00:00:00".into()
    } else {
        String::new()
    };
    *text = format!("{}{suffix}", date.format("%Y-%m-%d"));
}

#[derive(Clone, Copy, Default, PartialEq)]
enum CalendarView {
    #[default]
    Days,
    Months,
    Years,
}

fn dropdown_indicator(ui: &Ui, response: &Response) {
    let center = egui::pos2(response.rect.right() - 10.0, response.rect.center().y);
    ui.painter().add(egui::Shape::line(
        vec![
            center + egui::vec2(-3.0, -1.5),
            center + egui::vec2(0.0, 1.5),
            center + egui::vec2(3.0, -1.5),
        ],
        egui::Stroke::new(1.0, theme::TEXT_MUTED),
    ));
}

fn year_page(year: i32) -> i32 {
    (year.clamp(1, 9999) - 1) / 24 * 24 + 1
}

/// Returns the text response and whether the calendar owns this interaction.
pub(super) fn editor(ui: &mut Ui, text: &mut String, type_name: &str) -> (Response, bool) {
    let dated = has_date(type_name);
    let timed = has_time(type_name);
    let temporal = dated || timed;
    // Reserve the button inside the cell, including the right inset.
    let mut rect = ui.available_rect_before_wrap();
    rect.max.x -= 4.0;
    rect.max.y = rect.min.y + ui.spacing().interact_size.y.min(rect.height());
    let mut input_rect = rect;
    if temporal {
        input_rect.max.x = (rect.max.x - 28.0).max(rect.min.x);
    }
    let out = ui.put(
        input_rect,
        egui::TextEdit::singleline(text)
            .font(egui::TextStyle::Monospace)
            .frame(egui::Frame::NONE)
            .min_size(egui::Vec2::ZERO)
            .desired_width(input_rect.width())
            .hint_text(if dated {
                "YYYY-MM-DD"
            } else if timed {
                "HH:MM:SS"
            } else {
                ""
            }),
    );
    if !temporal {
        return (out, false);
    }
    let label = if dated { "Choose Date" } else { "Choose Time" };
    let button_rect = egui::Rect::from_min_max(
        egui::pos2((rect.max.x - 24.0).max(rect.min.x), rect.min.y),
        rect.max,
    );
    let button = ui
        .put(button_rect, egui::Button::new("…").frame(false))
        .on_hover_text(label);
    button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let month_id = out.id.with("calendar_month");
    let initial = date_prefix(text).unwrap_or_else(|| chrono::Local::now().date_naive());
    let clicked = button.clicked();
    let mut month = ui.ctx().data_mut(|data| {
        if clicked || data.get_temp::<NaiveDate>(month_id).is_none() {
            data.insert_temp(month_id, initial.with_day(1).unwrap());
        }
        data.get_temp::<NaiveDate>(month_id).unwrap()
    });
    ui.ctx()
        .accesskit_node_builder(out.id, |node| node.set_label("Date and time"));
    let view_id = out.id.with("calendar_view");
    let page_id = out.id.with("calendar_year_page");
    let (mut view, mut page) = ui.ctx().data_mut(|data| {
        if clicked {
            data.insert_temp(view_id, CalendarView::Days);
            data.insert_temp(page_id, year_page(month.year()));
        }
        (
            data.get_temp::<CalendarView>(view_id).unwrap_or_default(),
            data.get_temp::<i32>(page_id)
                .unwrap_or_else(|| year_page(month.year())),
        )
    });
    let viewport = ui.ctx().content_rect();
    let below = (viewport.bottom() - rect.bottom() - 16.0).max(0.0);
    let above = (rect.top() - viewport.top() - 16.0).max(0.0);
    let opens_below = below >= 180.0 || below >= above;
    let available_height = if opens_below { below } else { above };
    let popup = egui::Popup::from_toggle_button_response(&button)
        .anchor(rect)
        .align(if opens_below {
            egui::emath::RectAlign::BOTTOM_START
        } else {
            egui::emath::RectAlign::TOP_START
        })
        .align_alternatives(&[])
        .gap(0.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(280.0);
    let active = popup.is_open() || clicked;
    let escaped =
        active && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    let mut chosen = None;
    let mut applied = false;
    popup.show(|ui| {
        ui.set_width(280.0);
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);
        egui::ScrollArea::vertical()
            .id_salt("calendar_body")
            .max_height((available_height - 40.0).max(40.0))
            .show(ui, |ui| {
                if dated {
                    ui.horizontal(|ui| {
                        let previous =
                            ui.add_sized([28.0, 26.0], egui::Button::new("‹").frame(false));
                        previous.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                "Previous Period",
                            )
                        });
                        if previous.clicked() {
                            match view {
                                CalendarView::Days => {
                                    if let Some(date) = month.checked_sub_months(Months::new(1)) {
                                        month = date;
                                    }
                                }
                                CalendarView::Months => {
                                    if let Some(date) = month.with_year((month.year() - 1).max(1)) {
                                        month = date;
                                    }
                                }
                                CalendarView::Years => page = (page - 24).max(1),
                            }
                        }
                        if view == CalendarView::Years {
                            ui.add_sized(
                                [224.0, 26.0],
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "{} - {}",
                                        page,
                                        (page + 23).min(9999)
                                    ))
                                    .size(13.0),
                                ),
                            );
                        } else {
                            let month_button = ui.add_sized(
                                [140.0, 26.0],
                                egui::Button::new(month.format("%B").to_string()).frame(false),
                            );
                            month_button.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    "Choose Month",
                                )
                            });
                            dropdown_indicator(ui, &month_button);
                            if month_button.clicked() {
                                view = if view == CalendarView::Months {
                                    CalendarView::Days
                                } else {
                                    CalendarView::Months
                                };
                            }
                            let year_button = ui.add_sized(
                                [84.0, 26.0],
                                egui::Button::new(month.year().to_string()).frame(false),
                            );
                            year_button.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    "Choose Year",
                                )
                            });
                            dropdown_indicator(ui, &year_button);
                            if year_button.clicked() {
                                page = year_page(month.year());
                                view = CalendarView::Years;
                            }
                        }
                        let next = ui.add_sized([28.0, 26.0], egui::Button::new("›").frame(false));
                        next.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Next Period")
                        });
                        if next.clicked() {
                            match view {
                                CalendarView::Days => {
                                    if let Some(date) = month.checked_add_months(Months::new(1)) {
                                        month = date;
                                    }
                                }
                                CalendarView::Months => {
                                    if let Some(date) =
                                        month.with_year((month.year() + 1).min(9999))
                                    {
                                        month = date;
                                    }
                                }
                                CalendarView::Years => page = (page + 24).min(year_page(9999)),
                            }
                        }
                    });
                    ui.allocate_ui_with_layout(
                        egui::vec2(280.0, 202.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_min_height(202.0);
                            match view {
                                CalendarView::Years => {
                                    for row in 0..6 {
                                        ui.horizontal(|ui| {
                                            for col in 0..4 {
                                                let year = page + row * 4 + col;
                                                if year <= 9999
                                                    && ui
                                                        .add_sized(
                                                            [70.0, 26.0],
                                                            egui::Button::new(year.to_string())
                                                                .frame(year == month.year())
                                                                .selected(year == month.year()),
                                                        )
                                                        .clicked()
                                                {
                                                    month = month.with_year(year).unwrap();
                                                    view = CalendarView::Months;
                                                }
                                            }
                                        });
                                    }
                                }
                                CalendarView::Months => {
                                    for row in 0..4 {
                                        ui.horizontal(|ui| {
                                            for col in 0..3 {
                                                let candidate =
                                                    month.with_month(row * 3 + col + 1).unwrap();
                                                if ui
                                                    .add_sized(
                                                        [280.0 / 3.0, 40.0],
                                                        egui::Button::new(
                                                            candidate.format("%B").to_string(),
                                                        )
                                                        .frame(candidate.month() == month.month())
                                                        .selected(
                                                            candidate.month() == month.month(),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    month = candidate;
                                                    view = CalendarView::Days;
                                                }
                                            }
                                        });
                                    }
                                }
                                CalendarView::Days => {
                                    ui.horizontal(|ui| {
                                        for label in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] {
                                            ui.add_sized(
                                                [40.0, 22.0],
                                                egui::Label::new(
                                                    egui::RichText::new(label)
                                                        .size(11.0)
                                                        .color(theme::TEXT_MUTED),
                                                ),
                                            );
                                        }
                                    });
                                    let offset = month.weekday().num_days_from_monday();
                                    let selected = date_prefix(text);
                                    let today = chrono::Local::now().date_naive();
                                    for week in 0..6 {
                                        if week > 0
                                            && month.with_day(week * 7 - offset + 1).is_none()
                                        {
                                            break;
                                        }
                                        ui.horizontal(|ui| {
                                            for weekday in 0..7 {
                                                let day = (week * 7 + weekday)
                                                    .checked_sub(offset)
                                                    .map(|day| day + 1);
                                                let date = day.and_then(|day| month.with_day(day));
                                                let (rect, response) = ui.allocate_exact_size(
                                                    egui::vec2(40.0, 26.0),
                                                    if date.is_some() {
                                                        egui::Sense::click()
                                                    } else {
                                                        egui::Sense::hover()
                                                    },
                                                );
                                                if let Some(date) = date {
                                                    response.widget_info(|| {
                                                        egui::WidgetInfo::labeled(
                                                            egui::WidgetType::Button,
                                                            true,
                                                            date.format("%Y-%m-%d").to_string(),
                                                        )
                                                    });
                                                    let fill = if selected == Some(date) {
                                                        theme::BG_SELECTED
                                                    } else if response.hovered()
                                                        || response.has_focus()
                                                    {
                                                        theme::BG_HOVER
                                                    } else {
                                                        egui::Color32::TRANSPARENT
                                                    };
                                                    let tile = rect.shrink2(egui::vec2(4.0, 1.0));
                                                    ui.painter().rect_filled(tile, 3.0, fill);
                                                    if date == today {
                                                        ui.painter().rect_stroke(
                                                            tile,
                                                            3.0,
                                                            egui::Stroke::new(1.0, theme::ACCENT),
                                                            egui::StrokeKind::Inside,
                                                        );
                                                    }
                                                    ui.painter().text(
                                                        rect.center(),
                                                        egui::Align2::CENTER_CENTER,
                                                        date.day().to_string(),
                                                        egui::FontId::proportional(13.0),
                                                        theme::TEXT,
                                                    );
                                                    if response.clicked() {
                                                        chosen = Some(date);
                                                    }
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        },
                    );
                }
                if let Some(date) = chosen {
                    replace_date(text, date, type_name);
                }
                if timed {
                    ui.separator();
                    time_fields(ui, text, type_name, dated);
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            if dated
                && ui
                    .add_sized(
                        [64.0, 24.0],
                        egui::Button::new(egui::RichText::new("Today").size(12.0)),
                    )
                    .clicked()
            {
                let today = chrono::Local::now().date_naive();
                month = today.with_day(1).unwrap();
                view = CalendarView::Days;
                chosen = Some(today);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                applied = ui
                    .add_sized(
                        [64.0, 24.0],
                        egui::Button::new(egui::RichText::new("Apply").size(12.0)),
                    )
                    .clicked();
            });
        });
        if (chosen.is_some() && !timed) || applied || escaped {
            ui.close();
        }
    });
    ui.ctx().data_mut(|data| {
        data.insert_temp(month_id, month);
        data.insert_temp(view_id, view);
        data.insert_temp(page_id, page);
    });
    if let Some(date) = chosen {
        replace_date(text, date, type_name);
    }
    if (chosen.is_some() && !timed) || applied || escaped {
        out.request_focus();
    }
    (out, active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_editor_time_components_preserve_precision_and_zone() {
        for (value, dated) in [
            ("2026-10-12T03:21:07.971+00:00", true),
            ("-123:45:56.123456", false),
            ("03:21:07Z", false),
        ] {
            let mut parts = TimeParts::parse(value, dated).unwrap();
            assert_eq!(parts.value(), value);
            parts.minute = "59".into();
            assert!(parts.value().contains(":59:"));
        }
        assert!(!has_time("DATE"));
        assert!(has_time("TIME(6)"));
        assert!(has_time("timetz"));
        assert!(has_time("TIMESTAMP WITH TIME ZONE"));
        assert!(!has_date("TIME"));
    }

    #[test]
    fn cell_editor_calendar_preserves_time_fraction_and_zone() {
        let date = NaiveDate::from_ymd_opt(2028, 2, 29).unwrap();
        for suffix in ["", " 12:34:56.123456+02:00", "T12:34:56Z"] {
            let mut text = format!("2026-10-19{suffix}");
            replace_date(&mut text, date, "timestamp");
            assert_eq!(text, format!("2028-02-29{suffix}"));
        }
        let mut text = String::new();
        replace_date(&mut text, date, "datetime");
        assert_eq!(text, "2028-02-29 00:00:00");
        assert!(has_date("TIMESTAMP WITH TIME ZONE"));
        assert!(!has_date("time"));
        assert!(!has_date("text"));
    }
}
