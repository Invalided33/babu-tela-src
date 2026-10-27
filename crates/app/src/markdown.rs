//! The small markdown messages use, as the old page's `md()` read it:
//! ```code blocks```, `inline code`, links, **bold**, *italic*,
//! __underline__, ~~strike~~, ||spoilers|| and @mentions of people the app
//! knows. Parsing is plain Rust (tested below); `show` draws the result.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, CursorIcon, Rect, Response, Sense, Stroke, Ui, Vec2, pos2};

use crate::theme::{self, *};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub mention: bool,
    /// Which spoiler of the message this text belongs to.
    pub spoiler: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String, Style),
    Code(String),
    Link(String),
    /// A code block, on its own lines.
    Pre(String),
}

/// Splits a message into pieces. `known` says whether a name belongs to
/// someone, since only those mentions are highlighted.
pub fn parse(text: &str, known: &dyn Fn(&str) -> bool) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut spoilers = 0;
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("```") else { break };
        let mut body = &after[..end];
        // An optional language name on the first line is dropped.
        if let Some(nl) = body.find('\n')
            && body[..nl].chars().all(|c| c.is_ascii_lowercase())
        {
            body = &body[nl + 1..];
        }
        if body.trim().is_empty() {
            // ``` ``` with nothing inside is not a block.
            inline(&rest[..start + 3], known, &mut spoilers, &mut out);
            rest = &rest[start + 3..];
            continue;
        }
        inline(&rest[..start], known, &mut spoilers, &mut out);
        out.push(Piece::Pre(body.trim().to_owned()));
        rest = &after[end + 3..];
    }
    inline(rest, known, &mut spoilers, &mut out);
    merge(out)
}

/// Joins neighbouring text with the same style, which keeps the layout
/// small.
fn merge(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for p in pieces {
        if let (Some(Piece::Text(prev, ps)), Piece::Text(t, s)) = (out.last_mut(), &p)
            && ps == s
        {
            prev.push_str(t);
            continue;
        }
        if matches!(&p, Piece::Text(t, _) if t.is_empty()) {
            continue;
        }
        out.push(p);
    }
    out
}

/// Inline code and links first: nothing inside them is formatted.
fn inline(text: &str, known: &dyn Fn(&str) -> bool, spoilers: &mut usize, out: &mut Vec<Piece>) {
    let mut rest = text;
    loop {
        let code = rest.find('`').and_then(|s| {
            let close = rest[s + 1..].find('`')? + s + 1;
            let body = &rest[s + 1..close];
            (!body.is_empty() && !body.contains('\n')).then_some((s, close))
        });
        let link = find_link(rest);
        match (code, link) {
            (Some((s, close)), l) if l.is_none_or(|(ls, _)| s < ls) => {
                styled(&rest[..s], Style::default(), known, spoilers, out);
                out.push(Piece::Code(rest[s + 1..close].to_owned()));
                rest = &rest[close + 1..];
            }
            (_, Some((s, e))) => {
                styled(&rest[..s], Style::default(), known, spoilers, out);
                out.push(Piece::Link(rest[s..e].to_owned()));
                rest = &rest[e..];
            }
            _ => {
                styled(rest, Style::default(), known, spoilers, out);
                return;
            }
        }
    }
}

/// `https?://` up to the next whitespace or `<`.
fn find_link(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = text[from..].find("http") {
        let s = from + i;
        let after = &text[s..];
        let scheme = if after.starts_with("https://") {
            8
        } else if after.starts_with("http://") {
            7
        } else {
            from = s + 4;
            continue;
        };
        let len = after[scheme..]
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(after.len() - scheme);
        if len == 0 {
            from = s + scheme;
            continue;
        }
        return Some((s, s + scheme + len));
    }
    None
}

/// The markers, each with what its content may not contain.
const MARKS: &[(&str, &[char])] = &[
    ("**", &['*']),
    ("__", &['_']),
    ("~~", &['~']),
    ("||", &['|']),
    ("*", &['*', '\n']),
];

fn apply(style: Style, mark: &str, spoilers: &mut usize) -> Style {
    let mut s = style;
    match mark {
        "**" => s.bold = true,
        "__" => s.underline = true,
        "~~" => s.strike = true,
        "||" => {
            s.spoiler = Some(*spoilers);
            *spoilers += 1;
        }
        _ => s.italic = true,
    }
    s
}

/// The earliest marker in `text` that has a valid closing one.
fn next_mark(text: &str) -> Option<(usize, &'static str, usize)> {
    let mut best: Option<(usize, &'static str, usize)> = None;
    for &(mark, forbidden) in MARKS {
        let mut from = 0;
        while let Some(i) = text[from..].find(mark) {
            let s = from + i;
            let body_start = s + mark.len();
            let close = text[body_start..].find(mark).map(|c| body_start + c);
            let valid = close.is_some_and(|c| {
                let body = &text[body_start..c];
                !body.is_empty() && !body.contains(forbidden)
            });
            if let (true, Some(c)) = (valid, close) {
                if best.is_none_or(|(bs, _, _)| s < bs) {
                    best = Some((s, mark, c));
                }
                break;
            }
            from = s + 1;
            if from >= text.len() {
                break;
            }
            while !text.is_char_boundary(from) {
                from += 1;
            }
        }
    }
    best
}

fn styled(
    text: &str,
    style: Style,
    known: &dyn Fn(&str) -> bool,
    spoilers: &mut usize,
    out: &mut Vec<Piece>,
) {
    let mut rest = text;
    while let Some((s, mark, close)) = next_mark(rest) {
        mentions(&rest[..s], style, known, out);
        let inner = apply(style, mark, spoilers);
        styled(&rest[s + mark.len()..close], inner, known, spoilers, out);
        rest = &rest[close + mark.len()..];
    }
    mentions(rest, style, known, out);
}

/// "@name" at the start or after whitespace, 3 to 32 of a-z, 0-9, _ and .
fn mentions(text: &str, style: Style, known: &dyn Fn(&str) -> bool, out: &mut Vec<Piece>) {
    let mut last = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // '@' is a single byte, so where it is the text can be cut; looking
        // back from anywhere else could land inside a letter like "ç".
        let at_start = bytes[i] == b'@'
            && (i == 0
                || text[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace));
        if at_start {
            let name_len = bytes[i + 1..]
                .iter()
                .take_while(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || **b == b'_' || **b == b'.'
                })
                .count();
            let name = &text[i + 1..i + 1 + name_len];
            if (3..=32).contains(&name_len) && known(name) {
                out.push(Piece::Text(text[last..i].to_owned(), style));
                out.push(Piece::Text(
                    format!("@{name}"),
                    Style {
                        mention: true,
                        ..style
                    },
                ));
                i += 1 + name_len;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push(Piece::Text(text[last..].to_owned(), style));
}

// ---------------------------------------------------------------- drawing

pub enum Click {
    Link(String),
    Spoiler(usize),
}

enum Target {
    Link(String),
    Spoiler(usize),
}

/// One stretch of a paragraph, laid out on its own: text (selectable, with
/// its links and opened spoilers to click), or a spoiler not opened yet,
/// drawn as its box and kept out of the selection (selecting it would show
/// it, and Ctrl+C would copy it).
struct Run {
    job: LayoutJob,
    targets: Vec<(std::ops::Range<usize>, Target)>,
    /// The closed spoiler this is.
    hidden: Option<usize>,
}

/// The run the next text goes in: the last one, when it is of the same
/// kind (the same closed spoiler, or text).
fn run_for<'a>(ui: &Ui, runs: &'a mut Vec<Run>, hidden: Option<usize>) -> &'a mut Run {
    if runs.last().is_none_or(|r| r.hidden != hidden) {
        runs.push(Run {
            job: new_job(ui),
            targets: Vec::new(),
            hidden,
        });
    }
    runs.last_mut().expect("um trecho acabou de entrar")
}

/// Draws a parsed message. `revealed(i)` says whether spoiler `i` was
/// clicked open; `edited` adds the small "(editado)" at the end.
pub fn show(
    ui: &mut Ui,
    pieces: &[Piece],
    revealed: &dyn Fn(usize) -> bool,
    edited: bool,
) -> Option<Click> {
    let mut click = None;
    let mut runs: Vec<Run> = Vec::new();
    let last_text = pieces.iter().rposition(|p| !matches!(p, Piece::Pre(_)));
    for (i, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Pre(code) => {
                flush(ui, &mut runs, &mut click);
                egui::Frame::new()
                    .fill(BG0)
                    .corner_radius(CornerRadius::same(RADIUS_SM))
                    .inner_margin(egui::Margin::same(10))
                    .outer_margin(egui::Margin::symmetric(0, 4))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(code).font(theme::mono()).color(TEXT),
                            )
                            .wrap(),
                        );
                    });
            }
            Piece::Text(text, style) => {
                let hidden = style.spoiler.filter(|s| !revealed(*s));
                let run = run_for(ui, &mut runs, hidden);
                let start = run.job.text.len();
                run.job.append(text, 0.0, format(style, revealed));
                if let Some(s) = style.spoiler {
                    run.targets
                        .push((start..run.job.text.len(), Target::Spoiler(s)));
                }
            }
            Piece::Code(code) => {
                let mut f = TextFormat::simple(theme::mono(), TEXT);
                f.background = BG0;
                let run = run_for(ui, &mut runs, None);
                run.job.append(&format!("\u{2009}{code}\u{2009}"), 0.0, f);
            }
            Piece::Link(url) => {
                let run = run_for(ui, &mut runs, None);
                let start = run.job.text.len();
                let mut f = TextFormat::simple(theme::body(), ACCENT);
                f.underline = Stroke::new(1.0, ACCENT);
                run.job.append(url, 0.0, f);
                run.targets
                    .push((start..run.job.text.len(), Target::Link(url.clone())));
            }
        }
        if Some(i) == last_text && edited {
            run_for(ui, &mut runs, None).job.append(
                " (editado)",
                0.0,
                TextFormat::simple(egui::FontId::proportional(10.0), FAINT),
            );
        }
    }
    if pieces.is_empty() && edited {
        run_for(ui, &mut runs, None).job.append(
            "(editado)",
            0.0,
            TextFormat::simple(egui::FontId::proportional(10.0), FAINT),
        );
    }
    flush(ui, &mut runs, &mut click);
    click
}

fn new_job(ui: &Ui) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width().max(40.0);
    job
}

fn format(style: &Style, revealed: &dyn Fn(usize) -> bool) -> TextFormat {
    let font = if style.bold || style.mention {
        theme::bold(14.0)
    } else {
        theme::body()
    };
    let mut color = if style.mention { ACCENT } else { TEXT };
    let mut f = TextFormat::simple(font, color);
    f.italics = style.italic;
    if style.mention {
        f.background = ACCENT_SOFT;
    }
    if let Some(s) = style.spoiler {
        if revealed(s) {
            f.background = BG4;
        } else {
            f.background = BG0;
            color = Color32::TRANSPARENT;
            f.color = color;
        }
    }
    if style.underline {
        f.underline = Stroke::new(1.0, color);
    }
    if style.strike {
        f.strikethrough = Stroke::new(1.0, color);
    }
    f
}

/// Lays a paragraph's runs out one after the other, each picking up where
/// the one before stopped (as egui's labels do in a wrapping row), and
/// draws them. The text is selectable as egui's labels are: dragging
/// selects (across messages too) and Ctrl+C copies; a touch screen drags
/// to scroll instead, as a label does there. Links and spoilers open with
/// a click.
fn flush(ui: &mut Ui, runs: &mut Vec<Run>, click: &mut Option<Click>) {
    runs.retain(|r| !r.job.text.is_empty());
    if runs.is_empty() {
        return;
    }
    let origin = ui.cursor().min;
    let width = ui.available_width().max(40.0);
    let selectable = ui.style().interaction.selectable_labels;
    let drag = selectable && !ui.input(|i| i.has_touch_screen());
    // Where the last row so far ends, and how tall it is.
    let mut after: Option<(f32, f32)> = None;
    let mut top = origin.y;
    let mut size = Vec2::new(1.0, 1.0);
    for (n, run) in runs.drain(..).enumerate() {
        let mut job = run.job;
        job.wrap.max_width = width;
        if let Some((x, height)) = after {
            job.first_row_min_height = height;
            if let Some(first) = job.sections.first_mut() {
                first.leading_space = x;
            }
        }
        let galley = ui.painter().layout_job(job);
        let pos = pos2(origin.x, top);
        let mut sense = if run.targets.is_empty() {
            Sense::hover()
        } else {
            Sense::click()
        };
        if drag && run.hidden.is_none() {
            sense |= Sense::CLICK | Sense::DRAG;
        }
        // One response for its rows; the first one without the space it
        // picks up after.
        let mut resp: Option<Response> = None;
        for (r, row) in galley.rows.iter().enumerate() {
            let rect = if r == 0 {
                row.rect_without_leading_space()
            } else {
                row.rect()
            };
            let id = ui.auto_id_with(("markdown", n, r));
            let row_resp = ui.interact(rect.translate(pos.to_vec2()), id, sense);
            resp = Some(match resp {
                Some(all) => all | row_resp,
                None => row_resp,
            });
        }
        let Some(resp) = resp else { continue };
        let hit = resp.hover_pos().and_then(|at| {
            let cursor = galley.cursor_from_pos(at - pos);
            let byte = galley
                .text()
                .char_indices()
                .nth(cursor.index.0)
                .map_or(galley.text().len(), |(b, _)| b);
            run.targets.iter().find(|(r, _)| r.contains(&byte))
        });
        let clicked = hit.map(|(_, target)| match target {
            Target::Link(u) => Click::Link(u.clone()),
            Target::Spoiler(s) => Click::Spoiler(*s),
        });
        if let Some(last) = galley.rows.last() {
            let row = last.rect();
            after = Some((row.max.x, row.height()));
            top = pos.y + row.min.y;
        }
        size = size.max(Vec2::new(
            galley.size().x,
            pos.y + galley.size().y - origin.y,
        ));
        if ui.is_rect_visible(resp.rect) {
            if selectable && run.hidden.is_none() {
                // It also paints the galley, with the selection on it.
                egui::text_selection::LabelSelectionState::label_text_selection(
                    ui,
                    &resp,
                    pos,
                    galley,
                    TEXT,
                    Stroke::NONE,
                );
            } else {
                ui.painter().galley(pos, galley, TEXT);
            }
        }
        if let Some(target) = clicked {
            // After the selection, which asks for the text cursor.
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            if resp.clicked() {
                *click = Some(target);
            }
        }
    }
    ui.allocate_rect(Rect::from_min_size(origin, size), Sense::hover());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Vec<Piece> {
        parse(text, &|name| name == "ana" || name == "beto.silva")
    }

    fn plain(t: &str) -> Piece {
        Piece::Text(t.into(), Style::default())
    }

    #[test]
    fn plain_text_stays_one_piece() {
        assert_eq!(p("oi, tudo bem?"), vec![plain("oi, tudo bem?")]);
        assert_eq!(p(""), vec![]);
    }

    #[test]
    fn the_inline_styles() {
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        assert_eq!(
            p("a **b** c"),
            vec![plain("a "), Piece::Text("b".into(), bold), plain(" c")]
        );
        let it = Style {
            italic: true,
            ..Style::default()
        };
        assert_eq!(p("*x*"), vec![Piece::Text("x".into(), it)]);
        let u = Style {
            underline: true,
            ..Style::default()
        };
        assert_eq!(p("__x__"), vec![Piece::Text("x".into(), u)]);
        let s = Style {
            strike: true,
            ..Style::default()
        };
        assert_eq!(p("~~x~~"), vec![Piece::Text("x".into(), s)]);
        // Unclosed markers stay as they are.
        assert_eq!(p("2 * 3 = 6"), vec![plain("2 * 3 = 6")]);
        assert_eq!(p("**sem fim"), vec![plain("**sem fim")]);
    }

    #[test]
    fn spoilers_are_numbered_in_order() {
        let out = p("||a|| e ||b||");
        let ids: Vec<Option<usize>> = out
            .iter()
            .filter_map(|x| match x {
                Piece::Text(_, s) => s.spoiler.map(Some),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![Some(0), Some(1)]);
    }

    #[test]
    fn code_and_links_are_not_formatted_inside() {
        assert_eq!(
            p("use `**x**` aqui"),
            vec![plain("use "), Piece::Code("**x**".into()), plain(" aqui")]
        );
        assert_eq!(
            p("veja https://a.com/x_y_z agora"),
            vec![
                plain("veja "),
                Piece::Link("https://a.com/x_y_z".into()),
                plain(" agora")
            ]
        );
        assert_eq!(p("http:// sozinho"), vec![plain("http:// sozinho")]);
    }

    #[test]
    fn code_blocks_drop_the_language_line() {
        assert_eq!(
            p("antes\n```rust\nfn main() {}\n```depois"),
            vec![
                plain("antes\n"),
                Piece::Pre("fn main() {}".into()),
                plain("depois")
            ]
        );
        assert_eq!(p("```\n  x  \n```"), vec![Piece::Pre("x".into())]);
    }

    #[test]
    fn only_known_people_are_mentioned() {
        let m = Style {
            mention: true,
            ..Style::default()
        };
        assert_eq!(
            p("oi @ana"),
            vec![plain("oi "), Piece::Text("@ana".into(), m)]
        );
        assert_eq!(
            p("@beto.silva!"),
            vec![Piece::Text("@beto.silva".into(), m), plain("!")]
        );
        assert_eq!(p("oi @zeca"), vec![plain("oi @zeca")]);
        // Glued to a word is an e-mail, not a mention.
        assert_eq!(p("ana@ana"), vec![plain("ana@ana")]);
    }

    #[test]
    fn accents_and_emoji_survive() {
        assert_eq!(
            p("ação **ótimo** 👍"),
            vec![
                plain("ação "),
                Piece::Text(
                    "ótimo".into(),
                    Style {
                        bold: true,
                        ..Style::default()
                    }
                ),
                plain(" 👍"),
            ]
        );
        // A marker after a multi-byte character does not split it.
        assert_eq!(p("é*"), vec![plain("é*")]);
    }
}
