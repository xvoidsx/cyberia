use crate::matrix::matrix::Matrix;
use crate::matrix::roomcache::DecoratedRoom;
use crate::widgets::splash::{RainColumn, XorShift32, rain_color};
use crate::{close, consumed};
use crossterm::event::{KeyCode, KeyEvent};
use matrix_sdk::room::Room;
use matrix_sdk::ruma::MilliSecondsSinceUnixEpoch;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, ListState, StatefulWidget, Widget,
};
use std::cell::{Cell, RefCell};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::widgets::EventResult::Consumed;
use crate::widgets::get_margin;
use crate::widgets::textinput::TextInput;

use super::EventResult;
use crate::theme::{palette, role};

/// Subtle background rain for the hub — sparser and dimmer than the splash.
struct HubRain {
    cols: Vec<RainColumn>,
    rng: XorShift32,
    size: Option<(u16, u16)>,
    tick: u64,
}

impl HubRain {
    fn new() -> Self {
        Self {
            cols: Vec::new(),
            rng: XorShift32(0x5EED),
            size: None,
            tick: 0,
        }
    }

    /// Advance one frame of physics. Field borrows split cleanly here.
    fn advance(&mut self, height: u16, width: u16) {
        let (cols, rng) = (&mut self.cols, &mut self.rng);
        for col in cols.iter_mut() {
            col.substep(rng, height, width);
        }
        self.tick += 1;
    }
}

pub struct Rooms {
    pub textinput: TextInput,
    pub room: Vec<DecoratedRoom>,
    pub list_state: Cell<ListState>,
    rain: RefCell<HubRain>,
}

impl Rooms {
    pub fn new(matrix: Matrix, current: Option<Room>) -> Self {
        let mut rooms = matrix.fetch_rooms();
        sort_rooms(&mut rooms);

        // if the current room is at the top, put it at the bottom
        if let Some(current) = current
            && rooms.len() > 1
            && rooms.first().unwrap().inner.room_id() == current.room_id()
        {
            let first = rooms.remove(0);
            rooms.push(first);
        }

        let mut ret = Self {
            textinput: TextInput::new("Search".to_string(), true, false),
            room: rooms,
            list_state: Cell::new(ListState::default()),
            rain: RefCell::new(HubRain::new()),
        };

        ret.reset();
        ret
    }

    /// Advance the hub rain. Called from render, AFTER the content is
    /// drawn (see RoomsWidget::render): the don't-stomp guard below is
    /// only meaningful in that order, because once the text is down,
    /// a cell that is still exactly default-empty is genuinely empty —
    /// so rain falls through the whole picker, visible in every empty
    /// space, and never lands on text. The guard also requires the
    /// right neighbour to be empty, because katakana glyphs are wide
    /// and render over the next cell too. Drawing it first with a block
    /// exclusion (as 1.1.7 did) confined visible rain to the 1-row
    /// margins, which is why it looked stuck at the top.
    fn tick_rain(&self, area: Rect, buf: &mut Buffer) {
        let mut rain = self.rain.borrow_mut();

        // (re)init on resize
        if rain.size != Some((area.width, area.height)) {
            rain.size = Some((area.width, area.height));
            // sparse: one column per 6 cells
            let n = (area.width / 6).max(1) as usize;
            let mut cols = Vec::with_capacity(n);
            for i in 0..n {
                let mut c = RainColumn::new(&mut rain.rng, area.width, true);
                c.x = ((i as u16 * 6) + rain.rng.below(3) as u16) % area.width;
                // slower, calmer than the splash
                c.speed *= 0.5;
                cols.push(c);
            }
            rain.cols = cols;
        }

        rain.tick += 1;

        // advance physics (single substep — the hub is calm)
        rain.advance(area.height, area.width);

        // draw, dimmed to a whisper so the room list stays readable
        for col in &rain.cols {
            for (i, g) in col.glyphs.iter().enumerate() {
                let y = (col.head_y - i as f32).round() as i32;
                if y < 0 || y >= area.height as i32 {
                    continue;
                }
                let y = y as u16;
                let t = i as f32 / col.len.max(1) as f32;
                // push the ramp darker: hub rain is atmosphere, not theater
                let color = match rain_color(t) {
                    Color::Rgb(r, g, b) => Color::Rgb(r / 4, g / 4, b / 4),
                    c => c,
                };
                // Only into cells the content left exactly alone: a
                // space with default colors and no modifier. Text cells
                // carry a color, the highlight marker carries a color,
                // borders carry a glyph — all of them fail this.
                //
                // And the cell to the RIGHT must be empty too: a
                // katakana glyph is WIDE, rendering over its own cell
                // and the next one. The old guard checked only the
                // target cell, so a glyph landing in an empty cell left
                // of a letter swallowed that letter on the real
                // terminal — while the buffer still held the letter, so
                // the terminal diff never rewrote it and the hole
                // persisted. A wide glyph never reaches left, so the
                // right neighbour is the only one that matters; glyphs
                // at the last column are skipped outright.
                let target = &buf[(col.x, y)];
                let target_empty = target.symbol() == " "
                    && target.fg == Color::Reset
                    && target.bg == Color::Reset
                    && target.modifier.is_empty();
                let neighbour_empty = col.x + 1 < area.width && {
                    let n = &buf[(col.x + 1, y)];
                    n.symbol() == " "
                        && n.fg == Color::Reset
                        && n.bg == Color::Reset
                        && n.modifier.is_empty()
                };
                if target_empty && neighbour_empty {
                    let cell = &mut buf[(col.x, y)];
                    cell.set_char(*g);
                    cell.set_style(Style::default().fg(color));
                }
            }
        }
    }

    /// Big CYBERIA header, steady neon pink. The rain behind it
    /// provides the motion — no glitch on the title itself.
    fn draw_hub_title(&self, area: Rect, buf: &mut Buffer, y: u16) -> u16 {
        const TITLE: [char; 7] = ['C', 'Y', 'B', 'E', 'R', 'I', 'A'];

        let title_w: u16 = 13; // "C Y B E R I A"
        let x0 = area.width.saturating_sub(title_w) / 2;
        let style = Style::default()
            .fg(palette::NEON_PINK)
            .add_modifier(Modifier::BOLD);

        for (i, ch) in TITLE.iter().enumerate() {
            let x = x0 + i as u16 * 2;
            if x < area.width && y < area.height {
                let cell = &mut buf[(x, y)];
                cell.set_char(*ch);
                cell.set_style(style);
            }
        }

        // tagline underneath
        let tag = "enter the wired";
        let tx0 = area.width.saturating_sub(tag.len() as u16) / 2;
        if y + 1 < area.height {
            buf.set_string(
                tx0,
                y + 1,
                tag,
                Style::default().fg(Color::Rgb(0, 180, 180)),
            );
        }

        y + 3
    }

    pub fn widget(&self) -> RoomsWidget<'_> {
        RoomsWidget { rooms: self }
    }

    pub fn paste_event(&mut self, value: &str) -> EventResult {
        if let Consumed(_) = self.textinput.paste_event(value) {
            self.reset();
            consumed!()
        } else {
            EventResult::Ignored
        }
    }

    pub fn key_event(&mut self, input: &KeyEvent) -> EventResult {
        match input.code {
            KeyCode::Esc => close!(),
            KeyCode::Down => {
                self.next();
                consumed!()
            }
            KeyCode::Up => {
                self.previous();
                consumed!()
            }
            KeyCode::Enter => {
                if let Some(selected_room) = self.selected_room() {
                    let room = selected_room.inner();
                    Consumed(Box::new(|app| {
                        app.select_room(room);
                        app.close_popup();
                    }))
                } else {
                    EventResult::Ignored
                }
            }
            _ => {
                if let Consumed(_) = self.textinput.key_event(input) {
                    self.reset();
                    consumed!()
                } else {
                    EventResult::Ignored
                }
            }
        }
    }

    fn next(&mut self) {
        let len = self.filtered_rooms().len();

        if len == 0 {
            return;
        }

        let mut state = self.list_state.take();

        let i = match state.selected() {
            Some(i) => {
                if i >= len - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };

        state.select(Some(i));
        self.list_state.set(state);
    }

    fn previous(&mut self) {
        let len = self.filtered_rooms().len();

        if len == 0 {
            return;
        }

        let mut state = self.list_state.take();

        let i = match state.selected() {
            Some(i) => {
                if i == 0 {
                    len - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };

        state.select(Some(i));
        self.list_state.set(state);
    }

    fn reset(&mut self) {
        let mut state = self.list_state.take();
        state.select(Some(0));
        self.list_state.set(state);
    }

    fn filtered_rooms(&self) -> Vec<&DecoratedRoom> {
        let pattern = self.textinput.value.to_lowercase();

        self.room
            .iter()
            .filter(|j| j.name.to_string().to_lowercase().contains(pattern.as_str()))
            .collect()
    }

    fn selected_room(&self) -> Option<DecoratedRoom> {
        let filtered_rooms = self.filtered_rooms();

        if filtered_rooms.is_empty() {
            return None;
        }

        match self.list_state.take().selected() {
            Some(i) => Some(filtered_rooms[i].clone()),
            None => Some(filtered_rooms[0].clone()),
        }
    }
}

pub struct RoomsWidget<'a> {
    pub rooms: &'a Rooms,
}

impl Widget for RoomsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // clear first: the buffer persists across frames, and without this
        // old rain glyphs accumulate into dark smudges over the text
        buf.merge(&Buffer::empty(area));

        // The popup is fullscreen, so these area-relative coordinates are
        // absolute buffer coordinates.
        let full = area;
        let block_area = Layout::default()
            .direction(Direction::Horizontal)
            .vertical_margin(1)
            .horizontal_margin(get_margin(area.width, 70))
            .constraints([Constraint::Percentage(100)].as_ref())
            .split(area)[0];

        let area = block_area;

        // Render the main block (rain stays outside it, in the margins)
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(role::border());

        block.render(area, buf);

        // inner layout: title, search, room list
        let inner = Layout::default()
            .direction(Direction::Vertical)
            .vertical_margin(1)
            .horizontal_margin(3)
            .constraints([Constraint::Percentage(100)].as_ref())
            .split(area)[0];

        // hub title (drawn over the block)
        let list_y = self.rooms.draw_hub_title(inner, buf, inner.y);

        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(3),
                    Constraint::Min(1),
                ]
                .as_ref(),
            )
            .split(Rect::new(inner.x, list_y, inner.width, inner.height.saturating_sub(list_y - inner.y)));

        self.rooms.textinput.widget().render(splits[0], buf);

        let items: Vec<ListItem> = self
            .rooms
            .filtered_rooms()
            .into_iter()
            .map(make_list_item)
            .collect();

        let area = Layout::default()
            .horizontal_margin(1)
            .constraints([Constraint::Percentage(100)].as_ref())
            .split(splits[1])[0];

        let mut list_state = self.rooms.list_state.take();
        let list = List::new(items).highlight_style(role::highlight_symbol()).highlight_symbol("> ");
        StatefulWidget::render(list, area, buf, &mut list_state);
        self.rooms.list_state.set(list_state);

        // rain last, into empty cells only (see tick_rain): the content
        // is already down, so rain falls through the whole picker and
        // never touches text — and it touches no list state at all, so
        // the picker itself stays static while the rain moves.
        self.rooms.tick_rain(full, buf);
    }
}

/// "2m", "1h", "3d" — relative timestamp for the hub list.
fn rel_time(ts: MilliSecondsSinceUnixEpoch) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let then_ms = u64::from(ts.0) as i64;
    let diff_s = (now_ms - then_ms).max(0) / 1000;

    if diff_s < 60 {
        "now".to_string()
    } else if diff_s < 3600 {
        format!("{}m", diff_s / 60)
    } else if diff_s < 86400 {
        format!("{}h", diff_s / 3600)
    } else {
        format!("{}d", diff_s / 86400)
    }
}

fn make_list_item(room: &DecoratedRoom) -> ListItem<'_> {
    let name = room.name.to_string();
    let unread = room.unread_count();
    let highlights = room.highlight_count();

    let mut spans = vec![Span::styled(name, Style::default().add_modifier(Modifier::BOLD))];

    if unread > 0 {
        spans.push(Span::styled(
            format!(" ({})", unread),
            Style::default().fg(palette::NEON_PINK).add_modifier(Modifier::BOLD),
        ));
    } else if highlights > 0 {
        spans.push(Span::styled(
            format!(" ({})", highlights),
            role::accent_bold(),
        ));
    }

    // right-aligned timestamp
    if let Some(ts) = room.last_ts {
        spans.push(Span::styled(
            format!("  {}", rel_time(ts)),
            role::dim(),
        ));
    }

    let mut lines = Text::from(Line::from(spans));

    let preview = match (room.last_sender.as_ref(), room.last_message.as_ref()) {
        (Some(sender), Some(msg)) => {
            // single line, collapse newlines, truncate
            let clean: String = msg.replace('\n', " ").chars().take(80).collect();
            format!("{}: {}", sender, clean)
        }
        _ => String::new(),
    };
    lines.extend(Text::from(Line::from(vec![Span::styled(
        preview,
        role::dim(),
    )])));

    ListItem::new(lines)
}

pub fn sort_rooms(rooms: &mut [DecoratedRoom]) {
    rooms.sort_by_key(|r| (r.unread_count(), r.last_ts));
    rooms.reverse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Rendering the picker must never move selection or scroll offset:
    /// rain is decoration only and the picker stays static.
    #[test]
    fn picker_render_keeps_list_state_static() {
        let rooms = Rooms::new(Matrix::default(), None);
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let snapshot = |rooms: &Rooms| {
            let s = rooms.list_state.take();
            let v = (s.selected(), s.offset());
            rooms.list_state.set(s);
            v
        };
        // render twice first: the first render may normalize state for
        // an empty list, but after that it must be stable
        for _ in 0..2 {
            term.draw(|f| f.render_widget(rooms.widget(), f.area()))
                .unwrap();
        }
        let before = snapshot(&rooms);
        term.draw(|f| f.render_widget(rooms.widget(), f.area()))
            .unwrap();
        let after = snapshot(&rooms);
        assert_eq!(before, after);
    }

    fn ts_now_minus(secs: i64) -> MilliSecondsSinceUnixEpoch {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        MilliSecondsSinceUnixEpoch(ruma_uint(secs, now_ms))
    }

    fn ruma_uint(secs: i64, now_ms: i64) -> matrix_sdk::ruma::UInt {
        use std::str::FromStr;
        matrix_sdk::ruma::UInt::from_str(&((now_ms - secs * 1000).max(0).to_string())).unwrap()
    }

    #[test]
    fn rel_time_formats() {
        assert_eq!(rel_time(ts_now_minus(10)), "now");
        assert_eq!(rel_time(ts_now_minus(120)), "2m");
        assert_eq!(rel_time(ts_now_minus(3700)), "1h");
        assert_eq!(rel_time(ts_now_minus(90000)), "1d");
    }
}
