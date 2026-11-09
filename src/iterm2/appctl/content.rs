//! A pane's selection, copy, marks, open and find on iTerm2 (`SelectionRequest`, `GetBufferRequest`, menu items
//! run in the pane). Rows are absolute as the CLI counts them (0: the oldest line iTerm2 still has); iTerm2's
//! own line numbers also count the lines dropped off the top of the scrollback, so each request finds that
//! origin first.

use crate::app::wire::{ContentAct, Grab};
use crate::app::{Done, Fail, Id, Kind};

use super::super::proto::{self as p, client_originated_message::Submessage as Req, server_originated_message::Submessage as Resp};
use super::super::{unsupported, Live};
use super::{internal, invalid, menu, rpc};

/// One line of the buffer: its text, and each cell's byte span in it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub text: String,
    pub cells: Vec<(usize, usize)>,
}

impl Line {
    /// From iTerm2's text and its code points per cell (a cell may hold 0, 1 or several code points).
    pub fn new(text: String, cpc: &[(i32, i32)]) -> Line {
        let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
        let mut cells = Vec::new();
        let mut ci = 0usize; // code point index
        for &(n, reps) in cpc {
            for _ in 0..reps.max(0) {
                let a = starts[ci.min(starts.len() - 1)];
                ci += n.max(0) as usize;
                let b = starts[ci.min(starts.len() - 1)];
                cells.push((a, b - a));
            }
        }
        if cpc.is_empty() {
            // no map: a cell per code point
            cells = text.char_indices().map(|(i, c)| (i, c.len_utf8())).collect();
        }
        Line { text, cells }
    }

    /// Text of cells `from..to` (end exclusive, clamped).
    pub fn slice(&self, from: usize, to: usize) -> &str {
        let to = to.min(self.cells.len());
        if from >= to {
            return "";
        }
        let a = self.cells[from].0;
        let (s, l) = self.cells[to - 1];
        &self.text[a..s + l]
    }

    /// The cell holding byte `b` of the text.
    pub fn cell_of(&self, b: usize) -> usize {
        self.cells.iter().position(|(s, l)| b >= *s && b < s + l.max(&1)).unwrap_or(self.cells.len())
    }
}

/// The word around cell `col`: (from, to) cells, end exclusive. A word is letters, digits and `_-./~:@%+#?=&`.
pub fn word_at(l: &Line, col: usize) -> Option<(usize, usize)> {
    let is_word = |c: usize| l.slice(c, c + 1).chars().next().is_some_and(|ch| ch.is_alphanumeric() || "_-./~:@%+#?=&".contains(ch));
    if col >= l.cells.len() || !is_word(col) {
        return None;
    }
    let mut a = col;
    while a > 0 && is_word(a - 1) {
        a -= 1;
    }
    let mut b = col + 1;
    while b < l.cells.len() && is_word(b) {
        b += 1;
    }
    Some((a, b))
}

/// The newest match of `re` (bottom row first, the rightmost match in a row): (row index, from cell, to cell).
pub fn newest_match(lines: &[Line], re: &regex::Regex) -> Option<(usize, usize, usize)> {
    for (i, l) in lines.iter().enumerate().rev() {
        if let Some(m) = re.find_iter(&l.text).last() {
            return Some((i, l.cell_of(m.start()), l.cell_of(m.end().saturating_sub(1)) + 1));
        }
    }
    None
}

/// A buffer read: the absolute (iTerm2) line number of `lines[0]`, the lines, the cursor (x, absolute y).
struct Buf {
    first: i64,
    lines: Vec<Line>,
    cursor: Option<(i32, i64)>,
}

fn buffer(live: &mut Live, uuid: &str, range: p::LineRange) -> Result<Buf, Fail> {
    let req = p::GetBufferRequest { session: Some(uuid.into()), line_range: Some(range), include_styles: Some(false) };
    match rpc(live, Req::GetBufferRequest(req))? {
        Resp::GetBufferResponse(r) if r.status.unwrap_or(0) == 0 => {
            let first = r.windowed_coord_range.as_ref().and_then(|w| w.coord_range.as_ref()).and_then(|c| c.start.as_ref()).and_then(|s| s.y).unwrap_or(0);
            let lines = r
                .contents
                .into_iter()
                .map(|c| {
                    let cpc: Vec<(i32, i32)> = c.code_points_per_cell.iter().map(|x| (x.num_code_points.unwrap_or(1), x.repeats.unwrap_or(1))).collect();
                    Line::new(c.text.unwrap_or_default(), &cpc)
                })
                .collect();
            let cursor = r.cursor.map(|c| (c.x.unwrap_or(0), c.y.unwrap_or(0)));
            Ok(Buf { first, lines, cursor })
        }
        Resp::GetBufferResponse(r) => Err(invalid(format!("could not read the pane (status {})", r.status.unwrap_or(0)))),
        _ => Err(internal("unexpected reply to get buffer")),
    }
}

fn rows(y0: i64, n: i64) -> p::LineRange {
    let c = |y: i64| p::Coord { x: Some(0), y: Some(y) };
    p::LineRange {
        screen_contents_only: None,
        trailing_lines: None,
        windowed_coord_range: Some(p::WindowedCoordRange { coord_range: Some(p::CoordRange { start: Some(c(y0)), end: Some(c(y0 + n)) }), columns: None }),
    }
}

/// Everything iTerm2 still has (history and screen).
fn all(live: &mut Live, uuid: &str) -> Result<Buf, Fail> {
    buffer(live, uuid, p::LineRange { screen_contents_only: None, trailing_lines: Some(i32::MAX), windowed_coord_range: None })
}

/// iTerm2's line number of the CLI's row 0: the first line it still has.
fn origin(live: &mut Live, uuid: &str) -> Result<i64, Fail> {
    // the whole buffer's first line (costly with a long scrollback: iTerm2 has no cheaper answer for the lines it
    // dropped off the top)
    Ok(all(live, uuid)?.first)
}

/// A span in iTerm2's coordinates: (x, y) to (x, y), end exclusive in x.
type Span = ((i32, i64), (i32, i64));

fn set_selection(live: &mut Live, uuid: &str, s: Span) -> Result<(), Fail> {
    let c = |(x, y): (i32, i64)| p::Coord { x: Some(x), y: Some(y) };
    let sub = p::SubSelection {
        windowed_coord_range: Some(p::WindowedCoordRange { coord_range: Some(p::CoordRange { start: Some(c(s.0)), end: Some(c(s.1)) }), columns: None }),
        selection_mode: Some(p::SelectionMode::Character as i32),
        connected: Some(false),
    };
    let req = p::SelectionRequest {
        request: Some(p::selection_request::Request::SetSelectionRequest(p::selection_request::SetSelectionRequest {
            session_id: Some(uuid.into()),
            selection: Some(p::Selection { sub_selections: vec![sub] }),
        })),
    };
    match rpc(live, Req::SelectionRequest(req))? {
        Resp::SelectionResponse(r) if r.status.unwrap_or(0) == 0 => Ok(()),
        Resp::SelectionResponse(r) => Err(invalid(format!("iTerm2 refused the selection (status {})", r.status.unwrap_or(0)))),
        _ => Err(internal("unexpected reply to set selection")),
    }
}

fn get_selection(live: &mut Live, uuid: &str) -> Result<Option<Span>, Fail> {
    let req = p::SelectionRequest {
        request: Some(p::selection_request::Request::GetSelectionRequest(p::selection_request::GetSelectionRequest { session_id: Some(uuid.into()) })),
    };
    match rpc(live, Req::SelectionRequest(req))? {
        Resp::SelectionResponse(r) if r.status.unwrap_or(0) == 0 => {
            let Some(p::selection_response::Response::GetSelectionResponse(g)) = r.response else { return Ok(None) };
            let subs = g.selection.map(|s| s.sub_selections).unwrap_or_default();
            let spans: Vec<Span> = subs
                .iter()
                .filter_map(|s| {
                    let c = s.windowed_coord_range.as_ref()?.coord_range.as_ref()?;
                    let (a, b) = (c.start.as_ref()?, c.end.as_ref()?);
                    Some(((a.x.unwrap_or(0), a.y.unwrap_or(0)), (b.x.unwrap_or(0), b.y.unwrap_or(0))))
                })
                .collect();
            Ok(spans.first().map(|f| (f.0, spans.last().map_or(f.1, |l| l.1))))
        }
        Resp::SelectionResponse(r) => Err(invalid(format!("could not read the selection (status {})", r.status.unwrap_or(0)))),
        _ => Err(internal("unexpected reply to get selection")),
    }
}

/// The text of a span (iTerm2 coordinates), its lines joined by newlines.
fn span_text(live: &mut Live, uuid: &str, s: Span) -> Result<String, Fail> {
    let b = buffer(live, uuid, rows(s.0 .1, s.1 .1 - s.0 .1 + 1))?;
    let mut out = Vec::new();
    for (i, l) in b.lines.iter().enumerate() {
        let y = b.first + i as i64;
        let from = if y == s.0 .1 { s.0 .0.max(0) as usize } else { 0 };
        let to = if y == s.1 .1 { s.1 .0.max(0) as usize } else { usize::MAX };
        out.push(l.slice(from, to).trim_end().to_string());
    }
    Ok(out.join("\n"))
}

/// "sel\tR1:C1-R2:C2\tTEXT" (inclusive end, CLI rows).
fn sel_line(s: Span, origin: i64, text: &str) -> String {
    format!("sel\t{}:{}-{}:{}\t{}", s.0 .1 - origin, s.0 .0, s.1 .1 - origin, (s.1 .0 - 1).max(0), text)
}

/// A grab as a span in iTerm2's coordinates.
fn span_of(live: &mut Live, uuid: &str, g: &Grab) -> Result<Span, Fail> {
    let o = origin(live, uuid)?;
    let line = |live: &mut Live, row: u64| -> Result<Line, Fail> {
        buffer(live, uuid, rows(o + row as i64, 1))?.lines.into_iter().next().ok_or_else(|| invalid(format!("no row {row} in that pane")))
    };
    Ok(match g {
        Grab::Range((r1, c1), (r2, c2)) => {
            let end_x = if *c2 == u32::MAX { line(live, *r2)?.cells.len() as i32 } else { *c2 as i32 + 1 };
            ((*c1 as i32, o + *r1 as i64), (end_x, o + *r2 as i64))
        }
        Grab::Line(r) => {
            let l = line(live, *r)?;
            ((0, o + *r as i64), (l.cells.len() as i32, o + *r as i64))
        }
        Grab::Word(r, c) => {
            let l = line(live, *r)?;
            let (a, b) = word_at(&l, *c as usize).ok_or_else(|| invalid(format!("no word at {r}:{c}")))?;
            ((a as i32, o + *r as i64), (b as i32, o + *r as i64))
        }
        Grab::Match(re) => {
            let re = regex::Regex::new(re).map_err(|e| invalid(format!("--match: {e}")))?;
            let b = all(live, uuid)?;
            let (i, a, z) = newest_match(&b.lines, &re).ok_or_else(|| invalid(format!("no match for {} in that pane", re.as_str())))?;
            let y = b.first + i as i64;
            ((a as i32, y), (z as i32, y))
        }
    })
}

fn pbcopy(text: &str) -> Result<(), Fail> {
    use std::io::Write;
    let mut c = std::process::Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn().map_err(|e| internal(format!("pbcopy: {e}")))?;
    c.stdin.take().ok_or_else(|| internal("pbcopy: no stdin"))?.write_all(text.as_bytes()).map_err(|e| internal(format!("pbcopy: {e}")))?;
    c.wait().map_err(|e| internal(format!("pbcopy: {e}")))?;
    Ok(())
}

pub fn call(live: &mut Live, pane: Id, act: ContentAct) -> Result<Done, Fail> {
    let uuid = match live.ids.foreign(pane) {
        Some((Kind::Pane, u)) => u.to_string(),
        _ => return Err(Fail::new(crate::app::FailCode::NotFound, format!("no pane {pane}"))),
    };
    let done = |lines: Vec<String>| Ok(Done { ids: vec![pane], lines });
    match act {
        ContentAct::Get => {
            let o = origin(live, &uuid)?;
            match get_selection(live, &uuid)? {
                Some(s) => {
                    let t = span_text(live, &uuid, s)?;
                    done(vec![sel_line(s, o, &t)])
                }
                None => done(vec!["sel".into()]),
            }
        }
        ContentAct::Select(g) => {
            let s = span_of(live, &uuid, &g)?;
            set_selection(live, &uuid, s)?;
            let o = origin(live, &uuid)?;
            let t = span_text(live, &uuid, s)?;
            done(vec![sel_line(s, o, &t)])
        }
        ContentAct::Copy(g) => {
            let s = match g {
                Some(g) => span_of(live, &uuid, &g)?,
                None => get_selection(live, &uuid)?.ok_or_else(|| invalid("nothing selected in that pane (content copy --line R | --match RE …)"))?,
            };
            let t = span_text(live, &uuid, s)?;
            pbcopy(&t)?;
            done(vec![t])
        }
        ContentAct::Mark { row: None, name: None } => {
            super::run_item(live, menu::find("Marks and Annotations.Set Mark").map_err(invalid)?, Some(pane), false)?;
            let o = origin(live, &uuid)?;
            let b = buffer(live, &uuid, p::LineRange { screen_contents_only: Some(true), trailing_lines: None, windowed_coord_range: None })?;
            done(vec![b.cursor.map(|(_, y)| (y - o).to_string()).unwrap_or_default()])
        }
        ContentAct::Mark { .. } => Err(unsupported("marking a given row or by name (iTerm2 marks the cursor's line; named marks take its dialog)")),
        ContentAct::Jump(_) => Err(unsupported("jumping to a named mark (iTerm2's API does not list marks)")),
        ContentAct::Open(re) => {
            let re = regex::Regex::new(&re).map_err(|e| invalid(format!("--match: {e}")))?;
            let b = all(live, &uuid)?;
            let (i, a, z) = newest_match(&b.lines, &re).ok_or_else(|| invalid(format!("no match for {} in that pane", re.as_str())))?;
            let url = b.lines[i].slice(a, z).trim().to_string();
            if !openable(&url) {
                return Err(invalid(format!("{url:?} is not a URL or a path")));
            }
            // what ⌘-click does with a URL: hand it to the system
            let st = std::process::Command::new("/usr/bin/open").arg(&url).status().map_err(|e| internal(format!("open: {e}")))?;
            if !st.success() {
                return Err(invalid(format!("the system could not open {url:?}")));
            }
            done(vec![url])
        }
        ContentAct::FindBar(q) => {
            // the find bar takes its query from a selection (⌘E): select the newest match, then open the bar
            let s = span_of(live, &uuid, &Grab::Match(regex::escape(&q)))?;
            set_selection(live, &uuid, s)?;
            super::run_item(live, menu::find("Find.Use Selection for Find").map_err(invalid)?, Some(pane), false)?;
            super::run_item(live, menu::find("Find.Find...").map_err(invalid)?, Some(pane), false)?;
            done(vec![])
        }
    }
}

/// Screen text is anyone's: what a program printed, `open -aSomeApp` say, must never read as a flag of `open`.
fn openable(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-')
}

#[cfg(test)]
mod tests {
    #[test]
    fn screen_text_is_never_a_flag_of_open() {
        assert!(super::openable("https://example.com") && super::openable("/tmp/a b"));
        assert!(!super::openable("-aCalculator") && !super::openable("--args") && !super::openable(""));
    }

    use super::*;

    #[test]
    fn cells_words_and_matches() {
        // "xyz compañía": a blank cell after z, ñ and í as two code points each
        let l = Line::new("xyzcompan\u{303}i\u{301}a".into(), &[(1, 3), (0, 1), (1, 5), (2, 2), (1, 1)]);
        assert_eq!(l.cells.len(), 12);
        assert_eq!(l.slice(4, 12), "compan\u{303}i\u{301}a");
        assert_eq!(l.slice(0, 3), "xyz");
        assert_eq!(l.slice(3, 4), "");
        let plain = Line::new("see https://example.com/x now".into(), &[]);
        assert_eq!(word_at(&plain, 6), Some((4, 25)));
        assert_eq!(plain.slice(4, 25), "https://example.com/x");
        assert_eq!(word_at(&plain, 3), None);
        let lines = vec![Line::new("a https://one.example".into(), &[]), Line::new("b https://two.example c".into(), &[]), Line::new("".into(), &[])];
        let re = regex::Regex::new(r"https://\S+").unwrap();
        assert_eq!(newest_match(&lines, &re), Some((1, 2, 21)));
        assert_eq!(lines[1].slice(2, 21), "https://two.example");
        assert_eq!(sel_line(((2, 105), (7, 105)), 100, "hello"), "sel\t5:2-5:6\thello");
    }
}
