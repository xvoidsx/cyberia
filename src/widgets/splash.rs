//! Animated startup splash: katakana rain over nightshadeNeon theater.
//!
//! Full-screen on launch for ~8 ticks (~2s at the 250ms tick rate).
//! Any keypress dismisses it instantly. Pure render overlay — the
//! Matrix init in `App::tick` still runs underneath.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use crate::theme::palette;

/// Ticks (250ms each) the splash stays alive.
const TOTAL_FRAMES: usize = 8;
/// Physics substeps per rendered frame; the render rate is only 4fps,
/// so substeps keep the rain motion fluid.
const SUBSTEPS: usize = 3;
/// Rain columns are spaced 3 cells apart.
const COL_SPACING: u16 = 3;

/// The word, revealed letter by letter.
const TITLE: [char; 7] = ['C', 'Y', 'B', 'E', 'R', 'I', 'A'];
const TAGLINE: &str = "enter the wired";
/// Terminals smaller than this skip the splash entirely.
const MIN_WIDTH: u16 = 30;
const MIN_HEIGHT: u16 = 10;

/// Tiny inline xorshift32 — no new dependencies for a splash screen.
pub(crate) struct XorShift32(pub(crate) u32);

impl XorShift32 {
    pub(crate) fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    pub(crate) fn below(&mut self, n: u32) -> u32 {
        self.next() % n.max(1)
    }

    pub(crate) fn range_f32(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (self.next() as f32 / u32::MAX as f32) * (hi - lo)
    }
}

pub(crate) fn kana(rng: &mut XorShift32) -> char {
    char::from_u32(0x30A0 + rng.below(96)).unwrap_or('\u{30A0}')
}

pub(crate) struct RainColumn {
    pub(crate) x: u16,
    pub(crate) head_y: f32,
    pub(crate) speed: f32,
    pub(crate) len: u16,
    pub(crate) delay: u8,
    pub(crate) glyphs: Vec<char>,
}

impl RainColumn {
    pub(crate) fn new(rng: &mut XorShift32, width: u16, stagger: bool) -> Self {
        let len = 5 + rng.below(11) as u16;
        let glyphs = (0..len).map(|_| kana(rng)).collect();
        Self {
            x: (rng.below(width as u32) / 1).min(width.saturating_sub(1) as u32) as u16,
            head_y: -(rng.below(24) as f32),
            speed: rng.range_f32(0.4, 1.2),
            len,
            delay: if stagger { rng.below(6) as u8 } else { 0 },
            glyphs,
        }
    }

    pub(crate) fn respawn(&mut self, rng: &mut XorShift32, width: u16, height: u16) {
        *self = Self::new(rng, width, false);
        self.head_y = -(rng.below(10) as f32);
        self.delay = rng.below(4) as u8;
        let _ = height;
    }

    pub(crate) fn substep(&mut self, rng: &mut XorShift32, height: u16, width: u16) {
        if self.delay > 0 {
            self.delay -= 1;
            return;
        }
        self.head_y += self.speed;
        // occasional glyph flicker keeps the rain alive
        if rng.below(4) == 0 && !self.glyphs.is_empty() {
            let i = rng.below(self.glyphs.len() as u32) as usize;
            self.glyphs[i] = kana(rng);
        }
        if self.head_y - self.len as f32 > height as f32 {
            self.respawn(rng, width, height);
        }
    }
}

/// Rain color ramp: neon green head -> cyan -> dark teal -> black.
pub(crate) fn rain_color(t: f32) -> Color {
    const STOPS: [(f32, (u8, u8, u8)); 4] = [
        (0.0, (57, 255, 20)),   // neon green
        (0.35, (0, 255, 255)),  // cyan
        (0.7, (0, 100, 100)),   // dark teal
        (1.0, (0, 0, 0)),       // black
    ];
    let t = t.clamp(0.0, 1.0);
    let mut prev = STOPS[0];
    for &stop in &STOPS[1..] {
        if t <= stop.0 {
            let span = (stop.0 - prev.0).max(f32::EPSILON);
            let f = (t - prev.0) / span;
            let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f) as u8;
            return Color::Rgb(
                lerp(prev.1 .0, stop.1 .0),
                lerp(prev.1 .1, stop.1 .1),
                lerp(prev.1 .2, stop.1 .2),
            );
        }
        prev = stop;
    }
    Color::Rgb(0, 0, 0)
}

pub struct Splash {
    frame: usize,
    cols: Vec<RainColumn>,
    rng: XorShift32,
    size: Option<(u16, u16)>,
    /// Ambient mode: loops forever until a keypress dismisses it.
    /// Used for the on-demand screensaver (keybind `A`).
    ambient: bool,
    /// Unread activity for the ambient summary line.
    unread_total: u64,
    unread_rooms: usize,
}

impl Splash {
    pub fn new() -> Self {
        Self {
            frame: 0,
            cols: Vec::new(),
            rng: XorShift32(0xC0FFEE),
            size: None,
            ambient: false,
            unread_total: 0,
            unread_rooms: 0,
        }
    }

    /// Looping variant for ambient mode: title starts fully revealed,
    /// rain never stops, and `done()` stays false until dismissed.
    /// Shows a persistent unread-activity summary under the title.
    pub fn ambient(unread_total: u64, unread_rooms: usize) -> Self {
        Self {
            frame: TOTAL_FRAMES,
            cols: Vec::new(),
            rng: XorShift32(0xC0FFEE),
            size: None,
            ambient: true,
            unread_total,
            unread_rooms,
        }
    }

    /// Advance one 250ms tick. Call from `App::tick`.
    pub fn tick_frame(&mut self) {
        self.frame += 1;
    }

    pub fn done(&self) -> bool {
        !self.ambient && self.frame >= TOTAL_FRAMES
    }

    /// Letters revealed so far: one per tick, starting at tick 1.
    fn revealed(&self) -> usize {
        self.frame.min(TITLE.len())
    }

    fn ensure_init(&mut self, area: Rect) {
        if self.size == Some((area.width, area.height)) {
            return;
        }
        self.size = Some((area.width, area.height));
        let n = (area.width / COL_SPACING).max(1) as usize;
        self.cols = (0..n)
            .map(|i| {
                let mut c = RainColumn::new(&mut self.rng, area.width, true);
                // spread columns across the width instead of clustering
                c.x = ((i as u16 * COL_SPACING) + self.rng.below(3) as u16) % area.width;
                c
            })
            .collect();
    }

    /// Advance physics and paint the whole frame. Call from `App::render`
    /// instead of the normal UI while the splash is alive.
    pub fn render_frame(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.ensure_init(area);

        for _ in 0..SUBSTEPS {
            // borrow dance: rng and cols are both ours
            let (cols, rng) = (&mut self.cols, &mut self.rng);
            for col in cols.iter_mut() {
                col.substep(rng, area.height, area.width);
            }
        }

        let buf = frame.buffer_mut();
        // clear first: the buffer persists across frames, and without
        // this old rain glyphs accumulate over the text — the same
        // smudging the room list already clears for. Ambient mode runs
        // indefinitely, so it accumulates the longest.
        buf.merge(&Buffer::empty(area));
        self.draw_rain(buf, area);
        self.draw_title(buf, area);
        self.draw_tagline(buf, area);
        self.draw_unread_summary(buf, area);
        self.draw_footer(buf, area);
    }

    fn draw_rain(&self, buf: &mut Buffer, area: Rect) {
        let text_rows = Self::text_rows(area.height);
        for col in &self.cols {
            for (i, g) in col.glyphs.iter().enumerate() {
                let y = (col.head_y - i as f32).round() as i32;
                if y < 0 || y >= area.height as i32 {
                    continue;
                }
                // never on a text row: the rain stays visible everywhere
                // else without ever touching a letter
                if text_rows.contains(&(y as u16)) {
                    continue;
                }
                let t = i as f32 / col.len.max(1) as f32;
                let style = if i == 0 {
                    Style::default()
                        .fg(palette::NEON_GREEN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(rain_color(t))
                };
                let cell = &mut buf[(col.x, y as u16)];
                cell.set_char(*g);
                cell.set_style(style);
            }
        }
    }

    /// Rows that carry text (title, tagline, unread summary, footer).
    /// Rain never lands in these rows, so it can fall all around the
    /// text without ever interacting with a letter. The summary row is
    /// included even outside ambient mode, where nothing is drawn there
    /// — one less special case, and the band stays clean either way.
    fn text_rows(height: u16) -> [u16; 4] {
        let mid = height / 2;
        [
            mid.saturating_sub(1),
            mid + 1,
            mid + 3,
            height.saturating_sub(2),
        ]
    }

    fn draw_title(&mut self, buf: &mut Buffer, area: Rect) {
        // "C Y B E R I A" is 13 cells wide
        let title_w: u16 = 13;
        let x0 = area.width.saturating_sub(title_w) / 2;
        let y = area.height / 2 - 1;
        let revealed = self.revealed();
        for (i, ch) in TITLE.iter().enumerate() {
            let (glyph, style) = if i < revealed {
                // ambient mode: revealed letters glitch into katakana
                // and back, a few times a second
                if self.ambient && self.rng.below(14) == 0 {
                    let g = kana(&mut self.rng);
                    let s = Style::default()
                        .fg(palette::NEON_PINK)
                        .add_modifier(Modifier::BOLD);
                    (g, s)
                } else {
                    let s = Style::default()
                        .fg(palette::NEON_PINK)
                        .add_modifier(Modifier::BOLD);
                    (*ch, s)
                }
            } else {
                // dim ghost so the full word shape stays visible
                (*ch, Style::default().fg(Color::Rgb(85, 17, 51)))
            };
            let cell = &mut buf[(x0 + i as u16 * 2, y)];
            cell.set_char(glyph);
            cell.set_style(style);
        }
    }

    fn draw_tagline(&self, buf: &mut Buffer, area: Rect) {        // fades in over the last 4 ticks
        let step = match self.frame {
            4 => Some(Color::Rgb(0, 64, 64)),
            5 => Some(Color::Rgb(0, 128, 128)),
            6 => Some(Color::Rgb(0, 192, 192)),
            7.. => Some(palette::CYAN),
            _ => None,
        };
        let Some(color) = step else { return };
        let y = area.height / 2 + 1;
        let x0 = area
            .width
            .saturating_sub(TAGLINE.len() as u16)
            / 2;
        buf.set_string(x0, y, TAGLINE, Style::default().fg(color));
    }

    /// Persistent unread-activity summary for ambient mode.
    /// Present, not glitchy — a glanceable "is there anything new?"
    fn draw_unread_summary(&self, buf: &mut Buffer, area: Rect) {
        if !self.ambient {
            return;
        }

        let text = if self.unread_total == 0 {
            "[[ all quiet on the wired ]]".to_string()
        } else {
            let rword = if self.unread_rooms == 1 { "room" } else { "rooms" };
            let mword = if self.unread_total == 1 {
                "message"
            } else {
                "messages"
            };
            format!(
                "[[ {} {} in {} {} ]]",
                self.unread_total, mword, self.unread_rooms, rword
            )
        };

        let y = area.height / 2 + 3;
        if y >= area.height.saturating_sub(3) {
            return;
        }
        let x0 = area.width.saturating_sub(text.len() as u16) / 2;

        // pink brackets, white text
        let mut x = x0;
        for (i, ch) in text.chars().enumerate() {
            if x >= area.width {
                break;
            }
            let in_brackets = i < 3 || i >= text.chars().count() - 3;
            let style = if in_brackets {
                Style::default()
                    .fg(palette::NEON_PINK)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let cell = &mut buf[(x, y)];
            cell.set_char(ch);
            cell.set_style(style);
            x += 1;
        }
    }

    fn draw_footer(&self, buf: &mut Buffer, area: Rect) {
        // final 3 ticks (or always, in ambient mode)
        if !self.ambient && self.frame < 5 {
            return;
        }
        let text = if self.ambient {
            "ambient \u{2014} press any key".to_string()
        } else {
            format!("v{} \u{2014} press any key", env!("CARGO_PKG_VERSION"))
        };
        let y = area.height.saturating_sub(2);
        let x0 = area.width.saturating_sub(text.len() as u16) / 2;
        buf.set_string(
            x0,
            y,
            &text,
            Style::default().fg(Color::Rgb(110, 110, 110)),
        );
    }
}

impl Default for Splash {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimum-size guard shared with `App::render`: tiny terminals skip
/// the splash entirely instead of rendering garbage.
pub fn fits(area: Rect) -> bool {
    area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn xorshift_varies_and_bounds() {
        let mut rng = XorShift32(12345);
        let a = rng.next();
        let b = rng.next();
        assert_ne!(a, b);
        for _ in 0..100 {
            assert!(rng.below(96) < 96);
            let f = rng.range_f32(0.4, 1.2);
            assert!((0.4..=1.2).contains(&f));
        }
    }

    #[test]
    fn kana_in_katakana_block() {
        let mut rng = XorShift32(999);
        for _ in 0..200 {
            let c = kana(&mut rng);
            assert!(('\u{30A0}'..='\u{30FF}').contains(&c), "out of range: {c}");
        }
    }

    #[test]
    fn splash_completes_after_total_frames() {
        let mut s = Splash::new();
        for _ in 0..TOTAL_FRAMES - 1 {
            s.tick_frame();
            assert!(!s.done());
        }
        s.tick_frame();
        assert!(s.done());
    }

    #[test]
    fn title_reveals_over_time() {
        let mut s = Splash::new();
        assert_eq!(s.revealed(), 0);
        for expect in 1..=7 {
            s.tick_frame();
            assert_eq!(s.revealed(), expect);
        }
    }

    #[test]
    fn rain_wraps_at_bottom() {
        let mut s = Splash::new();
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| s.render_frame(f)).unwrap();
        assert!(!s.cols.is_empty());
        // shove a column far past the bottom; substeps must respawn it
        s.cols[0].head_y = 1000.0;
        s.cols[0].delay = 0;
        term.draw(|f| s.render_frame(f)).unwrap();
        assert!(s.cols[0].head_y < 24.0);
        for col in &s.cols {
            assert!(col.x < 80);
        }
    }

    #[test]
    fn tiny_terminal_fits_gate() {
        assert!(!fits(Rect::new(0, 0, 20, 24)));
        assert!(!fits(Rect::new(0, 0, 80, 5)));
        assert!(fits(Rect::new(0, 0, 30, 10)));
        // render itself must not panic on a small area either
        let mut s = Splash::new();
        let backend = TestBackend::new(20, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| s.render_frame(f)).unwrap();
    }

    #[test]
    fn rain_color_ramp_endpoints() {
        assert_eq!(rain_color(0.0), Color::Rgb(57, 255, 20));
        assert_eq!(rain_color(1.0), Color::Rgb(0, 0, 0));
        // midpoint-ish should be between green and cyan
        let mid = rain_color(0.2);
        assert!(mid != Color::Rgb(57, 255, 20));
    }


    #[test]
    fn ambient_rain_avoids_text_rows() {
        let mut s = Splash::ambient(4, 1);
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        for _ in 0..40 {
            s.tick_frame();
            term.draw(|f| s.render_frame(f)).unwrap();
        }
        let buf = term.backend().buffer().clone();
        let is_kana = |c: char| ('\u{30A0}'..='\u{30FF}').contains(&c);
        // tagline (13), summary (15), footer (22): no katakana at all
        for y in [13u16, 15, 22] {
            for x in 0..80 {
                let sym = buf[(x, y)].symbol();
                assert!(
                    !sym.chars().any(is_kana),
                    "rain on text row {y} at x={x}: {sym}"
                );
            }
        }
        // title row (11): katakana only where the designed glitch may
        // put it — on a title-letter cell, never between letters
        let x0 = (80u16 - 13) / 2;
        let title_xs: Vec<u16> = (0..7).map(|i| x0 + i * 2).collect();
        for x in 0..80 {
            let sym = buf[(x, 11)].symbol();
            if sym.chars().any(is_kana) {
                assert!(
                    title_xs.contains(&x),
                    "rain katakana on title row at non-title x={x}: {sym}"
                );
            }
        }
        // and the text itself survives intact
        let row_string = |y: u16| -> String {
            (0..80).map(|x| buf[(x, y)].symbol()).collect()
        };
        assert!(row_string(13).contains("enter the wired"));
        assert!(row_string(15).contains("[[ 4 messages in 1 room ]]"));
        assert!(row_string(22).contains("ambient"));
    }
    #[test]
    fn ambient_stores_unread_counts() {
        let s = Splash::ambient(5, 2);
        assert!(s.ambient);
        assert_eq!(s.unread_total, 5);
        assert_eq!(s.unread_rooms, 2);
        assert!(!s.done());
    }

    #[test]
    fn ambient_renders_summary_without_panic() {
        for (total, rooms) in [(0u64, 0usize), (1, 1), (42, 3)] {
            let mut s = Splash::ambient(total, rooms);
            let backend = TestBackend::new(80, 24);
            let mut term = Terminal::new(backend).unwrap();
            for _ in 0..10 {
                s.tick_frame();
                term.draw(|f| s.render_frame(f)).unwrap();
            }
        }
    }
}
