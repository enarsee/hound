//! Drawing the popup. Every text decision is a pure function of the app
//! state, the width and `now`; widths are measured in terminal cells.

use std::path::Path;

use hound_core::search::{Entry, Hit, Scope};
use ratatui::Frame;
use ratatui::buffer::CellWidth;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use super::app::App;
use crate::text::is_unprintable;

const ELLIPSIS: &str = "…";
const FILL: &str = "─";
const WARNING: &str = " ⚠ may be running elsewhere";
const SHORT_WARNING: &str = " ⚠";
/// Below this many cells for the title, the location and then the warning
/// text make way.
const MIN_TITLE: usize = 10;
/// Indent of a row's second line.
const DETAIL_INDENT: &str = "    ";
/// Narrowest truncated text worth showing beside the kept side of a bar.
const MIN_BAR_PART: usize = 5;
const HINTS: &str = "─ ⏎ open  tab scope  ^s subagents  esc close ";
/// The machine label of this machine's rows when remotes are configured.
const LOCAL_LABEL: &str = "local";
/// Widest machine label in a row's location, in cells; longer ones are cut.
const MAX_MACHINE: usize = 16;

/// The text of one result row, before fitting it to a width.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub live: bool,
    /// The live pane's agent status, for the marker's colour.
    pub status: String,
    pub harness: String,
    pub title: String,
    /// `<workspace> › <tab>` if live, else `<age>  <last cwd component>`.
    /// With a machine label: `<machine> › <workspace> › <tab>`, else
    /// `<age>  <machine> · <last cwd component>`.
    pub location: String,
    /// Closed, but changed so recently it may be running elsewhere.
    pub warning: bool,
    /// The snippet (`[subagent] ` first if it came from a subagent), else the
    /// session summary, else empty.
    pub detail: String,
}

/// The text of `hit`, whose entry is `entry`, without naming its machine.
pub fn row(hit: &Hit, entry: &Entry, now: i64) -> Row {
    build_row(hit, entry, now, None)
}

/// [`row`] with the location naming the entry's machine (`local` for this
/// one), the label on one line and cut to at most 16 cells.
pub fn machine_row(hit: &Hit, entry: &Entry, now: i64) -> Row {
    let label = machine_label(entry.machine.as_deref().unwrap_or(LOCAL_LABEL));
    build_row(hit, entry, now, Some(&label))
}

/// A machine name as shown: on one line, cut to at most 16 cells.
fn machine_label(machine: &str) -> String {
    truncate(&one_line(machine), MAX_MACHINE)
}

fn build_row(hit: &Hit, entry: &Entry, now: i64, machine: Option<&str>) -> Row {
    let session = entry.session.as_ref();
    let harness = match (session, &entry.live) {
        (Some(s), _) => s.key.harness.as_str().to_string(),
        (None, Some(l)) => l.kind.clone().unwrap_or_else(|| "shell".to_string()),
        (None, None) => String::new(),
    };
    let location = match (&entry.live, session) {
        (Some(l), _) => [
            machine.unwrap_or(""),
            l.workspace_label.as_str(),
            l.tab_label.as_str(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" › "),
        (None, Some(s)) => {
            let age = relative_age(now - entry.last_active());
            let dir = Path::new(&s.resume_cwd)
                .file_name()
                .map_or(s.resume_cwd.clone(), |d| d.to_string_lossy().into_owned());
            let dir = match machine {
                Some(m) if dir.is_empty() => m.to_string(),
                Some(m) => format!("{m} · {dir}"),
                None => dir,
            };
            if dir.is_empty() {
                age
            } else {
                format!("{age}  {dir}")
            }
        }
        (None, None) => String::new(),
    };
    let detail = match &hit.snippet {
        Some(snippet) if hit.from_subagent => format!("[subagent] {snippet}"),
        Some(snippet) => snippet.clone(),
        None => session.and_then(|s| s.summary.clone()).unwrap_or_default(),
    };
    Row {
        live: entry.is_live(),
        status: entry
            .live
            .as_ref()
            .map(|l| l.status.clone())
            .unwrap_or_default(),
        harness,
        title: one_line(entry.title()),
        location: one_line(&location),
        warning: entry.maybe_running_elsewhere(now),
        detail: one_line(&detail),
    }
}

/// The row's two lines, each at most `width` cells. Line 1 is marker,
/// harness, title (truncated first), warning, and the location right-aligned;
/// line 2 is the indented detail.
pub fn row_lines(row: &Row, width: usize) -> [Line<'static>; 2] {
    let (marker, marker_style) = if row.live {
        ("●", Style::new().fg(status_color(&row.status)))
    } else {
        ("○", Style::new().fg(Color::DarkGray))
    };
    let prefix_w = display_width(marker) + 1 + display_width(&harness_column(&row.harness));

    let mut location = row.location.as_str();
    let mut warning = if row.warning { WARNING } else { "" };
    let fixed = |location: &str, warning: &str| {
        let loc = if location.is_empty() {
            0
        } else {
            display_width(location) + 2
        };
        prefix_w + display_width(warning) + loc
    };
    if width.saturating_sub(fixed(location, warning)) < MIN_TITLE {
        location = "";
    }
    if width.saturating_sub(fixed(location, warning)) < MIN_TITLE && row.warning {
        warning = SHORT_WARNING;
    }
    let title = truncate(&row.title, width.saturating_sub(fixed(location, warning)));
    let used = prefix_w + display_width(&title) + display_width(warning) + display_width(location);
    let gap = " ".repeat(width.saturating_sub(used));

    let first = vec![
        Span::styled(marker.to_string(), marker_style),
        Span::raw(" "),
        Span::styled(harness_column(&row.harness), Style::new().fg(Color::Cyan)),
        Span::styled(title, Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(warning.to_string(), Style::new().fg(Color::Yellow)),
        Span::raw(gap),
        Span::styled(location.to_string(), Style::new().fg(Color::DarkGray)),
    ];
    let detail = truncate(&row.detail, width.saturating_sub(DETAIL_INDENT.len()));
    let second = vec![
        Span::raw(DETAIL_INDENT),
        Span::styled(detail, Style::new().fg(Color::Gray)),
    ];
    [
        Line::from(clip_spans(first, width)),
        Line::from(clip_spans(second, width)),
    ]
}

/// `"5m"`-style age of something `secs` seconds old.
pub fn relative_age(secs: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    match secs {
        s if s < MIN => "now".to_string(),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 14 * DAY => format!("{}d", s / DAY),
        s => format!("{}w", s / (7 * DAY)),
    }
}

/// Terminal cells `s` occupies, measured per grapheme as the buffer does.
/// Control characters count as zero.
pub fn display_width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

/// `s` cut to at most `width` cells, ending in `…` if anything was cut.
/// Never splits a grapheme.
pub fn truncate(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = take_width(s.graphemes(true), width - 1);
    out.push_str(ELLIPSIS);
    out
}

/// `s` cut from the front to at most `width` cells, starting with `…` if
/// anything was cut.
pub fn truncate_start(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut tail: Vec<&str> = Vec::new();
    for g in s.graphemes(true).rev() {
        used += grapheme_width(g);
        if used > width - 1 {
            break;
        }
        tail.push(g);
    }
    tail.reverse();
    format!("{ELLIPSIS}{}", tail.concat())
}

/// `left`, a `─` fill, and `right`, in exactly `width` cells (fewer only if
/// `width` cannot hold a single fill). When both do not fit, the side named by
/// `keep_right` keeps its text and the other is truncated or dropped.
pub fn bar(left: &str, right: &str, width: usize, keep_right: bool) -> String {
    let (lw, rw) = (display_width(left), display_width(right));
    let (left, right) = if lw + rw <= width {
        (left.to_string(), right.to_string())
    } else {
        let (kept, other) = if keep_right {
            (right, left)
        } else {
            (left, right)
        };
        let kept = truncate(kept, width);
        let room = width - display_width(&kept);
        let other = if room >= MIN_BAR_PART {
            truncate(other, room)
        } else {
            String::new()
        };
        if keep_right {
            (other, kept)
        } else {
            (kept, other)
        }
    };
    let fill = width.saturating_sub(display_width(&left) + display_width(&right));
    format!("{left}{}{right}", FILL.repeat(fill))
}

/// Header text: product name and scopes on the left, subagent state and
/// session count on the right.
pub fn header(app: &App, width: usize) -> String {
    let scopes: String = [
        (Scope::Active, "Active"),
        (Scope::Old, "Old"),
        (Scope::All, "All"),
    ]
    .into_iter()
    .map(|(scope, name)| {
        if scope == app.scope {
            format!("[{name}]")
        } else {
            format!(" {name} ")
        }
    })
    .collect();
    let left = format!(" hound ─ {} ", scopes.trim_end());
    let plural = if app.total_sessions == 1 { "" } else { "s" };
    let right = format!(
        " subagents: {} ─ {} session{plural} ",
        if app.include_subagents { "on" } else { "off" },
        app.total_sessions
    );
    bar(&left, &right, width, false)
}

/// Footer text: key hints on the left; on the right the status, else the
/// machine notes (`homelab: unreachable`) and indexing progress, joined
/// with two spaces.
pub fn footer(app: &App, width: usize) -> String {
    let right = match &app.status {
        Some(status) => format!(" {} ", one_line(status)),
        None => {
            let parts: Vec<String> = app
                .notes()
                .map(|(machine, note)| format!("{}: {}", machine_label(machine), one_line(note)))
                .chain(
                    app.progress
                        .map(|(done, total)| format!("indexing {done}/{total}")),
                )
                .collect();
            if parts.is_empty() {
                String::new()
            } else {
                format!(" {} ", parts.join("  "))
            }
        }
    };
    bar(HINTS, &right, width, true)
}

/// The query line, ` > query`, at most `width` cells with a cell to spare
/// for the cursor, and the cursor's column within it.
pub fn query_line(query: &str, width: usize) -> (String, usize) {
    let prefix = " > ";
    let room = width.saturating_sub(display_width(prefix) + 1);
    let line = format!("{prefix}{}", truncate_start(&one_line(query), room));
    let line = truncate(&line, width);
    let cursor = display_width(&line).min(width.saturating_sub(1));
    (line, cursor)
}

/// First hit to show so that `selected` is among `visible` rows.
pub fn scroll_offset(selected: usize, visible: usize) -> usize {
    if visible == 0 {
        0
    } else {
        (selected + 1).saturating_sub(visible)
    }
}

pub fn draw(frame: &mut Frame, app: &App, now: i64) {
    let area = frame.area();
    if area.width < 4 || area.height < 4 {
        let (line, cursor) = query_line(&app.query, area.width as usize);
        frame.render_widget(Paragraph::new(line), Rect { height: 1, ..area });
        frame.set_cursor_position((area.x + cursor as u16, area.y));
        return;
    }
    let inner_w = (area.width - 2) as usize;
    let row_at = |y: u16| Rect::new(area.x + 1, y, area.width - 2, 1);

    frame.render_widget(Block::bordered(), area);
    frame.render_widget(Paragraph::new(header(app, inner_w)), row_at(area.y));

    let (query, cursor) = query_line(&app.query, inner_w);
    frame.render_widget(Paragraph::new(query), row_at(area.y + 1));
    frame.set_cursor_position((area.x + 1 + cursor as u16, area.y + 1));

    let separator = format!("├{}┤", FILL.repeat(inner_w));
    frame.render_widget(
        Paragraph::new(separator),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );

    let list = Rect::new(area.x + 1, area.y + 3, area.width - 2, area.height - 4);
    draw_rows(frame, app, list, now);

    let footer_style = if app.status.is_some() {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(footer(app, inner_w)).style(footer_style),
        row_at(area.bottom() - 1),
    );
}

fn draw_rows(frame: &mut Frame, app: &App, list: Rect, now: i64) {
    if list.height == 0 {
        return;
    }
    let rows = app.rows();
    if rows.is_empty() {
        let message = if app.query.is_empty() {
            "No sessions"
        } else {
            "No sessions match"
        };
        let line = truncate(&format!("  {message}"), list.width as usize);
        frame.render_widget(
            Paragraph::new(line).style(Style::new().fg(Color::DarkGray)),
            Rect { height: 1, ..list },
        );
        return;
    }
    let visible = (list.height / 2) as usize;
    let offset = scroll_offset(app.selected, visible);
    for (slot, (i, (entry, hit))) in rows
        .into_iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + 2 * slot as u16, list.width, 2);
        // One cell of padding each side, when there is room for it.
        let text = if rect.width > 2 {
            Rect::new(rect.x + 1, rect.y, rect.width - 2, 2)
        } else {
            rect
        };
        let row = if app.remotes_configured {
            machine_row(hit, entry, now)
        } else {
            row(hit, entry, now)
        };
        let lines = row_lines(&row, text.width as usize);
        frame.render_widget(Paragraph::new(Vec::from(lines)), text);
        if i == app.selected {
            frame
                .buffer_mut()
                .set_style(rect, Style::new().add_modifier(Modifier::REVERSED));
        }
    }
}

/// The harness name padded to the column the sketch uses.
fn harness_column(harness: &str) -> String {
    format!("{harness:<6}  ")
}

fn status_color(status: &str) -> Color {
    match status {
        "working" => Color::Yellow,
        "blocked" => Color::Red,
        "done" => Color::Green,
        "idle" => Color::Blue,
        _ => Color::Green,
    }
}

/// `s` on one line: every [unprintable](is_unprintable) character (newline,
/// tab, escape, bidirectional and zero-width controls, …) becomes a space.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if is_unprintable(c) { ' ' } else { c })
        .collect()
}

fn grapheme_width(g: &str) -> usize {
    if g.chars().all(char::is_control) {
        0
    } else {
        g.cell_width() as usize
    }
}

/// Graphemes from `graphemes`, in order, while they fit in `width` cells.
fn take_width<'a>(graphemes: impl Iterator<Item = &'a str>, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for g in graphemes {
        let w = grapheme_width(g);
        if used + w > width {
            break;
        }
        used += w;
        out.push_str(g);
    }
    out
}

/// `spans` cut to at most `width` cells in total, without an ellipsis.
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut left = width;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        if left == 0 {
            break;
        }
        let content = take_width(span.content.graphemes(true), left);
        left -= display_width(&content);
        out.push(Span::styled(content, span.style));
    }
    out
}
