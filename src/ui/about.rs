//! About DBeast: an old-school cracktro screen with copper bars, a starfield,
//! a wobbling logo, the author's ASCII portrait, a sine scroller and a looped
//! chiptune. Animation runs only while the window is open; the music plays on
//! its own thread and its mute choice is saved in the configuration.

use super::{App, theme};
use egui::{
    Align2, Color32, FontId, Id, Rect, RichText, Sense, Stroke, Ui, Vec2, ecolor::Hsva, pos2, vec2,
};
use std::sync::mpsc;

pub const AUTHOR: &str = "Maciej Olszewski";
pub const AUTHOR_URL: &str = "https://github.com/macolszewski";
const PORTRAIT: &str = include_str!("../../assets/branding/author-ascii.txt");
/// "Copper Byte", an original 51.2 s chiptune loop composed for this screen.
const THEME: &[u8] = include_bytes!("../../assets/audio/copper-byte-loop.ogg");
const SIZE: Vec2 = vec2(800.0, 540.0);
/// Top and bottom of the portrait and credits panels, inside the screen.
const PANEL_TOP: f32 = 128.0;
const PANEL_BOTTOM: f32 = 74.0;
const STARS: usize = 160;
const SCROLL_TEXT: &str = "*** DBEAST *** ULTRA FAST DATABASE MANAGER *** CODE BY MACIEJ OLSZEWSKI *** \
    POSTGRESQL * MYSQL * SQLITE *** 100% NATIVE RUST, ZERO JVM *** INSTANT START, TINY MEMORY, \
    SMOOTH SCROLLING *** MUSIC: COPPER BYTE *** GREETINGS TO EVERY DBA, DATA ENGINEER AND QUERY \
    WIZARD OUT THERE *** STAY FAST ***          ";

#[derive(Default)]
pub(super) struct About {
    open: bool,
    /// Off in tests, so no audio device is ever opened there.
    sound: bool,
    music: Option<Music>,
    stars: Vec<Star>,
}

struct Star {
    x: f32,
    y: f32,
    z: f32,
}

/// The looped theme on a dedicated thread; dropping it stops playback and
/// closes the audio device.
struct Music {
    control: mpsc::Sender<bool>,
}

impl Music {
    fn start() -> Self {
        let (control, commands) = mpsc::channel::<bool>();
        let _ = std::thread::Builder::new().name("about-music".into()).spawn(move || {
            let Ok(mut sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
                return;
            };
            sink.log_on_drop(false);
            let Ok(source) = rodio::Decoder::new_looped(std::io::Cursor::new(THEME)) else {
                return;
            };
            let player = rodio::Player::connect_new(sink.mixer());
            player.set_volume(0.7);
            player.append(source);
            // true plays, false pauses; a closed channel ends playback.
            while let Ok(play) = commands.recv() {
                if play { player.play() } else { player.pause() }
            }
        });
        Self { control }
    }

    fn set_playing(&self, playing: bool) {
        let _ = self.control.send(playing);
    }
}

impl About {
    pub(super) fn new(sound: bool) -> Self {
        Self { sound, ..Default::default() }
    }
}

impl App {
    pub(super) fn open_about(&mut self) {
        let about = &mut self.about;
        about.open = true;
        if about.stars.is_empty() {
            // A fixed pseudo-random field, so the screen looks the same each time.
            let mut seed = 0x2545_f491_u32;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as f32 / u32::MAX as f32
            };
            about.stars = (0..STARS).map(|_| Star { x: next() * 2.0 - 1.0, y: next() * 2.0 - 1.0, z: next() }).collect();
        }
        if about.sound && !self.config.about_muted && about.music.is_none() {
            about.music = Some(Music::start());
        }
    }

    pub(super) fn about_open(&self) -> bool {
        self.about.open
    }

    fn set_about_muted(&mut self, muted: bool) {
        self.config.about_muted = muted;
        self.save_config();
        match (&self.about.music, muted) {
            (Some(music), _) => music.set_playing(!muted),
            (None, false) if self.about.sound => self.about.music = Some(Music::start()),
            (None, _) => {}
        }
    }

    pub(super) fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about.open {
            return;
        }
        let muted = self.config.about_muted;
        let mut toggle = false;
        let frame = egui::Frame::window(&ctx.global_style()).fill(Color32::BLACK).inner_margin(egui::Margin::symmetric(8, 6));
        let response = theme::Dialog::new(Id::new("about_dbeast"), "About DBeast")
            .resizable(false)
            .default_size(SIZE)
            .min_size(SIZE)
            .frame(frame)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(SIZE, Sense::hover());
                let time = ui.input(|i| i.time) as f32;
                let painter = ui.painter_at(rect);
                background(&painter, rect, time, &self.about.stars);
                logo(&painter, rect, time);
                portrait(&painter, rect, time);
                scroller(&painter, rect, time);
                toggle = credits(ui, rect, muted);
                scanlines(&painter, rect);
            });
        // The animation needs frames only while the screen is open.
        ctx.request_repaint();
        if toggle {
            self.set_about_muted(!muted);
        }
        if response.should_close() {
            self.about.open = false;
            self.about.music = None;
        }
    }
}

/// Black sky, a 3D starfield flying towards the viewer and Amiga-style copper bars.
fn background(painter: &egui::Painter, rect: Rect, time: f32, stars: &[Star]) {
    painter.rect_filled(rect, 0.0, Color32::from_rgb(4, 2, 14));
    let center = rect.center();
    for star in stars {
        let z = 1.0 - (star.z + time * 0.12).fract();
        let depth = z.max(0.02);
        let pos = center + vec2(star.x * rect.width() * 0.5, star.y * rect.height() * 0.5) / (depth * 2.4);
        if !rect.contains(pos) {
            continue;
        }
        let light = ((1.0 - z) * 255.0) as u8;
        painter.circle_filled(pos, 0.4 + (1.0 - z) * 1.6, Color32::from_rgb(light, light, light.saturating_add(40)));
    }
    const BAR: f32 = 26.0;
    for bar in 0..5 {
        let phase = bar as f32 * 0.55;
        let y = rect.top() + rect.height() * (0.5 + 0.38 * (time * 1.3 + phase).sin());
        let hue = (bar as f32 / 5.0 + time * 0.03).fract();
        for strip in 0..13 {
            let t = strip as f32 / 12.0;
            let shade = 1.0 - (t * 2.0 - 1.0).abs();
            let color: Color32 = Hsva::new(hue, 0.85, 0.15 + 0.85 * shade, 0.32).into();
            let top = y - BAR / 2.0 + t * BAR;
            painter.rect_filled(Rect::from_x_y_ranges(rect.x_range(), top..=top + BAR / 13.0 + 0.5), 0.0, color);
        }
    }
}

/// The DBEAST logo: chrome letters wobbling on a sine, cycling through the rainbow.
fn logo(painter: &egui::Painter, rect: Rect, time: f32) {
    const LETTERS: &str = "DBEAST";
    let font = theme::semibold(64.0);
    let advance = 52.0;
    let start = rect.center().x - advance * (LETTERS.len() as f32 - 1.0) / 2.0;
    for (i, letter) in LETTERS.chars().enumerate() {
        let x = start + i as f32 * advance;
        let y = rect.top() + 52.0 + (time * 3.0 + i as f32 * 0.7).sin() * 8.0;
        let glyph = painter.layout_no_wrap(letter.to_string(), font.clone(), Color32::PLACEHOLDER);
        let at = pos2(x, y) - glyph.size() / 2.0;
        painter.galley(at + vec2(4.0, 4.0), glyph.clone(), Color32::from_rgb(60, 0, 80));
        let hue = (i as f32 * 0.12 + time * 0.15).fract();
        painter.galley(at, glyph, Hsva::new(hue, 0.65, 1.0, 1.0).into());
    }
    let tagline = format!("ULTRA FAST DATABASE MANAGER  ·  v{}", env!("CARGO_PKG_VERSION"));
    painter.text(
        pos2(rect.center().x, rect.top() + 102.0),
        Align2::CENTER_CENTER,
        tagline,
        FontId::monospace(12.0),
        Color32::from_rgb(140, 220, 255),
    );
}

/// The author's portrait in phosphor-green ASCII, with a scanning beam.
fn portrait(painter: &egui::Painter, rect: Rect, time: f32) {
    let galley = painter.layout_no_wrap(PORTRAIT.to_string(), FontId::monospace(6.5), Color32::PLACEHOLDER);
    let frame = Rect::from_min_max(
        pos2(rect.left() + 28.0, rect.top() + PANEL_TOP),
        pos2(rect.left() + 44.0 + galley.size().x, rect.bottom() - PANEL_BOTTOM),
    );
    let area = Rect::from_center_size(frame.center(), galley.size());
    painter.rect_filled(frame, 6.0, Color32::from_black_alpha(200));
    painter.rect_stroke(frame, 6.0, Stroke::new(1.0, Color32::from_rgb(40, 140, 70)), egui::StrokeKind::Inside);
    let flicker = 0.9 + 0.1 * (time * 23.0).sin().abs();
    painter.galley(area.min, galley, Color32::from_rgb(70, (235.0 * flicker) as u8, 120));
    let beam = area.top() + (time * 0.35).fract() * area.height();
    painter.rect_filled(
        Rect::from_x_y_ranges(area.x_range(), beam..=(beam + 6.0).min(area.bottom())),
        0.0,
        Color32::from_rgba_unmultiplied(150, 255, 170, 36),
    );
}

/// The classic bottom scroller: each letter rides a sine wave in its own colour.
fn scroller(painter: &egui::Painter, rect: Rect, time: f32) {
    const SPACING: f32 = 17.0;
    let font = FontId::monospace(20.0);
    let base = rect.bottom() - 34.0;
    let length = SCROLL_TEXT.len() as f32 * SPACING;
    let offset = (time * 120.0) % length;
    painter.rect_filled(
        Rect::from_x_y_ranges(rect.x_range(), base - 24.0..=base + 24.0),
        0.0,
        Color32::from_black_alpha(140),
    );
    let first = (offset / SPACING) as usize;
    let visible = (rect.width() / SPACING) as usize + 2;
    for n in 0..visible {
        let index = (first + n) % SCROLL_TEXT.len();
        let letter = SCROLL_TEXT.as_bytes()[index] as char;
        if letter == ' ' {
            continue;
        }
        let x = rect.left() + (first + n) as f32 * SPACING - offset;
        let y = base + (x * 0.025 + time * 4.0).sin() * 10.0;
        let hue = (x / rect.width() + time * 0.2).fract();
        let glyph = painter.layout_no_wrap(letter.to_string(), font.clone(), Color32::PLACEHOLDER);
        painter.galley(pos2(x, y) - glyph.size() / 2.0, glyph, Hsva::new(hue, 0.8, 1.0, 1.0).into());
    }
}

/// The text panel on the right; returns true when the sound toggle was clicked.
fn credits(ui: &mut Ui, rect: Rect, muted: bool) -> bool {
    let panel = Rect::from_min_max(
        pos2(rect.left() + 300.0, rect.top() + PANEL_TOP),
        pos2(rect.right() - 28.0, rect.bottom() - PANEL_BOTTOM),
    );
    ui.painter().rect_filled(panel, 8.0, Color32::from_rgba_unmultiplied(8, 6, 30, 215));
    ui.painter().rect_stroke(panel, 8.0, Stroke::new(1.0, Color32::from_rgb(90, 70, 200)), egui::StrokeKind::Inside);
    let mut toggle = false;
    ui.scope_builder(egui::UiBuilder::new().max_rect(panel.shrink2(vec2(18.0, 14.0))), |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.label(RichText::new("DBeast").font(theme::semibold(20.0)).color(Color32::WHITE));
        ui.label(
            RichText::new(
                "A native database client for PostgreSQL, MySQL and SQLite, written from scratch in Rust. \
                 It starts instantly, stays light on memory and scrolls large results smoothly, with a \
                 SQL console, data editor, schema explorer, query plans, data transfer and a local MCP \
                 server for AI agents.",
            )
            .color(Color32::from_gray(205)),
        );
        ui.add_space(6.0);
        ui.label(RichText::new("CREATED BY").font(FontId::monospace(11.0)).color(Color32::from_rgb(255, 120, 200)));
        ui.label(RichText::new(AUTHOR).font(theme::semibold(16.0)).color(Color32::WHITE));
        ui.hyperlink_to(RichText::new("github.com/macolszewski").color(Color32::from_rgb(120, 200, 255)), AUTHOR_URL);
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!("Version {}  ·  Rust, egui, tokio", env!("CARGO_PKG_VERSION")))
                .small()
                .color(Color32::from_gray(150)),
        );
        ui.label(RichText::new("Music: \"Copper Byte\", original chiptune theme").small().color(Color32::from_gray(150)));
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            let (icon, label) = if muted { ("🔇", "Sound off") } else { ("🔊", "Sound on") };
            let button = egui::Button::new(RichText::new(format!("{icon}  {label}")).color(Color32::WHITE))
                .fill(Color32::from_rgb(40, 30, 90))
                .stroke(Stroke::new(1.0, Color32::from_rgb(120, 100, 230)));
            toggle = ui
                .add(button)
                .on_hover_text(if muted { "Unmute the music" } else { "Mute the music" })
                .clicked();
        });
    });
    toggle
}

/// CRT scanlines over the whole screen.
fn scanlines(painter: &egui::Painter, rect: Rect) {
    let stroke = Stroke::new(1.0, Color32::from_black_alpha(55));
    let mut y = rect.top() + 1.0;
    while y < rect.bottom() {
        painter.hline(rect.x_range(), y, stroke);
        y += 3.0;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn theme_and_portrait_are_embedded() {
        assert_eq!(&super::THEME[..4], b"OggS");
        assert!(super::PORTRAIT.lines().count() >= 30);
        assert!(super::SCROLL_TEXT.is_ascii(), "the scroller indexes bytes");
    }

    #[test]
    fn theme_decodes_as_a_48_khz_stereo_loop() {
        use rodio::Source;
        let mut source = rodio::Decoder::new_looped(std::io::Cursor::new(super::THEME)).unwrap();
        assert_eq!((source.sample_rate().get(), source.channels().get()), (48_000, 2));
        assert!(source.by_ref().take(48_000).any(|sample| sample != 0.0), "audible");
    }
}
