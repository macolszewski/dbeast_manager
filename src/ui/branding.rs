//! Embedded, pre-rasterized artwork: no SVG parsing or disk I/O at runtime.
use super::theme;
use egui::{Color32, RichText, TextureHandle, Ui};

pub fn window_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../../assets/branding/icon-128.png"))
        .expect("validated application icon")
        .into_rgba8();
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

/// A presentation timer, never a sleep on the UI thread.
#[derive(Default)]
pub(super) struct StartupSplash {
    active: bool,
    duration: f64,
    started: Option<f64>,
}

impl StartupSplash {
    pub(super) fn new(duration: f64) -> Self {
        Self {
            active: true,
            duration,
            started: None,
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> bool {
        if !self.active {
            return false;
        }
        let now = ctx.input(|i| i.time);
        let started = *self.started.get_or_insert(now);
        let remaining = (self.duration - (now - started)).max(0.0);
        let dismissed = ctx.input(|i| {
            i.pointer.any_pressed()
                || i.events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
        });
        if dismissed || remaining == 0.0 {
            // Keep this frame covered so the dismissing input cannot activate the workspace.
            self.active = false;
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(remaining));
        }
        true
    }
}

/// What the splash says under the artwork.
pub(super) enum SplashStatus<'a> {
    /// Startup presentation; any input continues.
    Starting,
    /// Configuration and saved passwords are being read (macOS may ask for Keychain access).
    Loading,
    /// Loading failed; the splash offers Retry.
    Failed(&'a str),
    /// Empty workspace background.
    Workspace,
}

#[derive(Default)]
pub(super) struct Branding {
    hero: Option<TextureHandle>,
    small: Option<TextureHandle>,
}

impl Branding {
    fn texture(
        slot: &mut Option<TextureHandle>,
        ui: &Ui,
        name: &'static str,
        bytes: &[u8],
    ) -> egui::TextureId {
        slot.get_or_insert_with(|| {
            let image = image::load_from_memory(bytes)
                .expect("validated branding asset")
                .into_rgba8();
            ui.ctx().load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            )
        })
        .id()
    }

    pub(super) fn thumbnail(&mut self, ui: &mut Ui) {
        let id = Self::texture(
            &mut self.small,
            ui,
            "dbeast-thumbnail",
            include_bytes!("../../assets/branding/icon-64.png"),
        );
        ui.add(egui::Image::new((id, egui::vec2(24.0, 24.0))).alt_text("DBeast icon"));
    }

    /// Returns true when Retry was clicked.
    pub(super) fn splash(&mut self, ui: &mut Ui, status: SplashStatus<'_>) -> bool {
        let mut retry = false;
        let id = Self::texture(
            &mut self.hero,
            ui,
            "dbeast-hero",
            include_bytes!("../../assets/branding/icon-512.png"),
        );
        let available = ui.available_size();
        let compact = available.y < 380.0;
        let icon_size = if compact { 96.0 } else { 160.0 };
        let height = match (&status, compact) {
            (SplashStatus::Workspace, true) => 300.0,
            (SplashStatus::Workspace, false) => 420.0,
            (_, true) => 260.0,
            (_, false) => 340.0,
        };
        ui.vertical_centered(|ui| {
            ui.add_space(((available.y - height) * 0.5).max(8.0));
            ui.add(
                egui::Image::new((id, egui::vec2(icon_size, icon_size)))
                    .alt_text("DBeast dragon and database"),
            );
            ui.add_space(18.0);
            ui.label(
                RichText::new("DBeast")
                    .font(theme::semibold(38.0))
                    .color(Color32::from_rgb(239, 239, 237)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("Ultra Fast Database Manager")
                    .size(15.0)
                    .color(Color32::from_rgb(190, 193, 197)),
            );
            ui.add_space(22.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(32.0, 2.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 1.0, Color32::from_rgb(239, 132, 72));
            ui.add_space(18.0);
            let hint = |text: &str| RichText::new(text).size(12.0).color(theme::TEXT_MUTED);
            match status {
                SplashStatus::Starting => {
                    ui.label(hint("Click or press any key to continue"));
                }
                SplashStatus::Workspace => workspace_shortcuts(ui, compact),
                SplashStatus::Loading => {
                    // Loading may wait for a Keychain prompt for minutes: animate at
                    // 4 frames per second instead of a spinner's full refresh rate.
                    let step = (ui.input(|i| i.time) * 4.0) as usize % 3;
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(36.0, 18.0), egui::Sense::hover());
                    for dot in 0..3 {
                        let color = if dot == step { theme::TEXT } else { theme::TEXT_MUTED.gamma_multiply(0.5) };
                        ui.painter().circle_filled(
                            egui::pos2(rect.left() + 6.0 + dot as f32 * 12.0, rect.center().y),
                            3.5,
                            color,
                        );
                    }
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Loading data sources and saved passwords…")
                            .size(13.0)
                            .color(Color32::from_rgb(190, 193, 197)),
                    );
                    ui.add_space(4.0);
                    ui.label(hint(if cfg!(target_os = "macos") { "macOS may ask to allow DBeast to use its keychain item." } else { "The desktop may ask to unlock the keyring." }));
                }
                SplashStatus::Failed(error) => {
                    ui.label(
                        RichText::new("Data sources could not be loaded")
                            .size(13.0)
                            .color(theme::RED),
                    );
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(hint(error)).wrap());
                    ui.add_space(12.0);
                    retry = ui.button("Retry").clicked();
                }
            }
        });
        retry
    }
}

/// Actions listed on the empty workspace, like the hints of an IDE's empty
/// editor: names right-aligned in muted text, shortcuts left-aligned beside them.
const WORKSPACE_SHORTCUTS: [(&str, &str); 5] = [
    ("New Query Console", "⌘⇧L"),
    ("Find Action", "⌘⇧A"),
    ("Database Explorer", "⌘1"),
    ("Files", "⌘2"),
    ("Services", "⌘8"),
];

fn workspace_shortcuts(ui: &mut Ui, compact: bool) {
    let rows: &[(&str, &str)] = if compact {
        &WORKSPACE_SHORTCUTS[..3]
    } else {
        &WORKSPACE_SHORTCUTS
    };
    let row_height = 24.0;
    let font = egui::FontId::proportional(13.0);
    for (name, keys) in rows {
        let keys = theme::keys(keys);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::hover(),
        );
        // One accessible row, so the names do not clash with panel titles.
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("{name}: {keys}"))
        });
        let painter = ui.painter();
        painter.text(
            egui::pos2(rect.center().x - 8.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            name,
            font.clone(),
            theme::TEXT_MUTED,
        );
        painter.text(
            egui::pos2(rect.center().x + 8.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            keys,
            font.clone(),
            theme::SQL_FUNCTION,
        );
    }
}

static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Called first thing in `main`; the start of the startup measurement.
pub fn mark_process_start() {
    PROCESS_START.get_or_init(std::time::Instant::now);
}

/// With `DBEAST_TRACE_STARTUP=1`, prints the time to the first frame once.
pub(super) fn trace_first_frame() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !DONE.swap(true, std::sync::atomic::Ordering::Relaxed)
        && std::env::var_os("DBEAST_TRACE_STARTUP").is_some()
        && let Some(start) = PROCESS_START.get()
    {
        eprintln!("dbeast: first frame after {} ms", start.elapsed().as_millis());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splash_artwork() {
        let mut branding = Branding::default();
        let mut first_frame = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size([720.0, 480.0])
            .with_pixels_per_point(2.0)
            .wgpu()
            .build_ui(|ui| {
                if std::mem::take(&mut first_frame) {
                    theme::apply(ui.ctx());
                    ui.ctx().request_repaint();
                    return;
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG_EDITOR))
                    .show(ui, |ui| branding.splash(ui, SplashStatus::Starting));
            });
        h.run_steps(3);
        h.render()
            .unwrap()
            .save("docs/screenshots/branding-splash.png")
            .unwrap();
    }

    #[test]
    fn loading_and_failure_stay_on_the_splash() {
        use egui_kittest::kittest::Queryable;
        struct State {
            branding: Branding,
            styled: bool,
            failed: bool,
            retried: bool,
        }
        let mut h = egui_kittest::Harness::builder()
            .with_size([1000.0, 700.0])
            .with_pixels_per_point(2.0)
            .wgpu()
            .build_ui_state(
                |ui, state: &mut State| {
                    if !std::mem::replace(&mut state.styled, true) {
                        theme::apply(ui.ctx());
                        ui.ctx().request_repaint();
                        return;
                    }
                    let status = if state.failed {
                        SplashStatus::Failed("Keychain access failed (OSStatus -128). Unlock the keychain or allow access, then retry.")
                    } else {
                        SplashStatus::Loading
                    };
                    egui::CentralPanel::default()
                        .frame(egui::Frame::new().fill(theme::BG_EDITOR))
                        .show(ui, |ui| state.retried |= state.branding.splash(ui, status));
                },
                State {
                    branding: Branding::default(),
                    styled: false,
                    failed: false,
                    retried: false,
                },
            );
        h.run_steps(3);
        assert!(h.query_by_label("Loading data sources and saved passwords…").is_some());
        h.render()
            .unwrap()
            .save("docs/screenshots/branding-loading.png")
            .unwrap();
        h.state_mut().failed = true;
        h.run_steps(3);
        h.render()
            .unwrap()
            .save("docs/screenshots/branding-load-failed.png")
            .unwrap();
        h.get_by_label("Retry").click();
        h.run_steps(2);
        assert!(h.state().retried);
    }

    #[test]
    fn measure_embedded_artwork_decode() {
        let start = std::time::Instant::now();
        for _ in 0..50 {
            let hero =
                image::load_from_memory(include_bytes!("../../assets/branding/icon-512.png"))
                    .unwrap()
                    .into_rgba8();
            let small =
                image::load_from_memory(include_bytes!("../../assets/branding/icon-64.png"))
                    .unwrap()
                    .into_rgba8();
            std::hint::black_box((hero, small, window_icon()));
        }
        eprintln!(
            "Branding PNG decode (hero + thumbnail + window), mean of 50: {:?}",
            start.elapsed() / 50
        );
    }
}
