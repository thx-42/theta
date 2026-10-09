//! Markdown -> pre-wrapped ratatui lines.

use super::theme;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn finish(mut spans: Vec<Span<'static>>) -> Line<'static> {
    while let Some(last) = spans.last() {
        let t = last.content.trim_end().to_string();
        if t.is_empty() && spans.len() > 1 {
            spans.pop();
            continue;
        }
        if let Some(l) = spans.last_mut() {
            l.content = t.into();
        }
        break;
    }
    Line::from(spans)
}

/// Word-wrap styled spans to `width` columns; `indent` prefixes continuation lines.
pub fn wrap(spans: Vec<Span<'static>>, width: usize, first: Vec<Span<'static>>, indent: Vec<Span<'static>>) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = first.clone();
    let mut cur_w: usize = first.iter().map(|s| s.content.width()).sum();
    let indent_w: usize = indent.iter().map(|s| s.content.width()).sum();
    let start_w = cur_w;
    let mut at_start = true;
    for span in spans {
        let style = span.style;
        // Split into words keeping whitespace attached as separate tokens.
        let text = span.content.to_string();
        let mut tokens: Vec<String> = Vec::new();
        let mut buf = String::new();
        for c in text.chars() {
            if c == '\n' {
                if !buf.is_empty() { tokens.push(std::mem::take(&mut buf)); }
                tokens.push("\n".into());
            } else if c == ' ' {
                if !buf.is_empty() && !buf.ends_with(' ') { tokens.push(std::mem::take(&mut buf)); }
                buf.push(c);
            } else {
                if buf.ends_with(' ') { tokens.push(std::mem::take(&mut buf)); }
                buf.push(c);
            }
        }
        if !buf.is_empty() { tokens.push(buf); }
        for tok in tokens {
            if tok == "\n" {
                lines.push(Line::from(std::mem::take(&mut cur)));
                cur = indent.clone();
                cur_w = indent_w;
                at_start = true;
                continue;
            }
            let w = tok.width();
            let is_space = tok.starts_with(' ');
            if cur_w + w > width && cur_w > indent_w.max(start_w) {
                lines.push(finish(std::mem::take(&mut cur)));
                cur = indent.clone();
                cur_w = indent_w;
                at_start = true;
                if is_space { continue; }
            }
            if is_space && at_start && cur_w == indent_w && !lines.is_empty() {
                continue;
            }
            if w > width.saturating_sub(cur_w) && !is_space {
                // Hard-break long tokens (URLs, paths).
                let mut piece = String::new();
                let mut pw = 0;
                for c in tok.chars() {
                    let cw = c.width().unwrap_or(0);
                    if cur_w + pw + cw > width && (pw > 0 || cur_w > indent_w) {
                        cur.push(Span::styled(std::mem::take(&mut piece), style));
                        lines.push(Line::from(std::mem::take(&mut cur)));
                        cur = indent.clone();
                        cur_w = indent_w;
                        pw = 0;
                    }
                    piece.push(c);
                    pw += cw;
                }
                cur.push(Span::styled(piece, style));
                cur_w += pw;
            } else {
                cur.push(Span::styled(tok, style));
                cur_w += w;
            }
            at_start = false;
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

struct R {
    width: usize,
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote: usize,
    item_first: bool,
    code: Option<(String, String)>,
    link: Option<String>,
    table: Option<Vec<Vec<String>>>,
    cell: String,
}

impl R {
    fn style(&self) -> Style {
        self.styles.iter().fold(Style::default(), |a, s| a.patch(*s))
    }

    fn prefix(&self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for _ in 0..self.quote {
            first.push(Span::styled("│ ", theme::dim()));
            rest.push(Span::styled("│ ", theme::dim()));
        }
        let depth = self.lists.len();
        if depth > 0 {
            let pad = "  ".repeat(depth - 1);
            let bullet = match self.lists.last().unwrap() {
                Some(n) => format!("{n}. "),
                None => "• ".to_string(),
            };
            let bw = bullet.width();
            if self.item_first {
                first.push(Span::raw(pad.clone()));
                first.push(Span::styled(bullet, theme::accent()));
            } else {
                first.push(Span::raw(format!("{pad}{}", " ".repeat(bw))));
            }
            rest.push(Span::raw(format!("{pad}{}", " ".repeat(bw))));
        }
        (first, rest)
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let (first, rest) = self.prefix();
        let spans = std::mem::take(&mut self.spans);
        self.out.extend(wrap(spans, self.width, first, rest));
        self.item_first = false;
    }

    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| l.width() > 0) && self.lists.is_empty() {
            self.out.push(Line::default());
        }
    }
}

pub fn render(md: &str, width: usize) -> Vec<Line<'static>> {
    let mut r = R { width, out: vec![], spans: vec![], styles: vec![], lists: vec![], quote: 0, item_first: false, code: None, link: None, table: None, cell: String::new() };
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    r.flush();
                    r.blank();
                    let st = match level {
                        HeadingLevel::H1 => theme::accent().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        HeadingLevel::H2 => theme::accent().add_modifier(Modifier::BOLD),
                        _ => Style::default().add_modifier(Modifier::BOLD),
                    };
                    r.styles.push(st);
                }
                Tag::Paragraph
                    if r.lists.is_empty() => {
                        r.flush();
                        r.blank();
                    }
                Tag::BlockQuote(_) => {
                    r.flush();
                    r.quote += 1;
                    r.styles.push(theme::dim().add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    r.flush();
                    r.blank();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    r.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    r.flush();
                    if r.lists.is_empty() {
                        r.blank();
                    }
                    r.lists.push(start);
                }
                Tag::Item => {
                    r.flush();
                    r.item_first = true;
                }
                Tag::Emphasis => r.styles.push(Style::default().add_modifier(Modifier::ITALIC)),
                Tag::Strong => r.styles.push(Style::default().add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => r.styles.push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    r.link = Some(dest_url.to_string());
                    r.styles.push(theme::link());
                }
                Tag::Table(_) => {
                    r.flush();
                    r.blank();
                    r.table = Some(vec![]);
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(t) = r.table.as_mut() {
                        t.push(vec![]);
                    }
                }
                Tag::TableCell => r.cell.clear(),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    r.flush();
                    r.styles.pop();
                }
                TagEnd::Paragraph => r.flush(),
                TagEnd::BlockQuote(_) => {
                    r.flush();
                    r.quote -= 1;
                    r.styles.pop();
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = r.code.take() {
                        let label = if lang.is_empty() { String::new() } else { format!(" {lang} ") };
                        r.out.push(Line::from(vec![Span::styled("╭─", theme::dim()), Span::styled(label, theme::dim())]));
                        for l in code.trim_end_matches('\n').lines() {
                            let mut first = vec![Span::styled("│ ", theme::dim())];
                            first.extend(r.prefix().1);
                            let indent = first.clone();
                            r.out.extend(wrap(vec![Span::styled(l.replace('\t', "    "), theme::code())], width, first, indent));
                        }
                        r.out.push(Line::from(Span::styled("╰─", theme::dim())));
                    }
                }
                TagEnd::List(_) => {
                    r.flush();
                    r.lists.pop();
                    if r.lists.is_empty() {
                        r.out.push(Line::default());
                    }
                }
                TagEnd::Item => {
                    r.flush();
                    if let Some(Some(n)) = r.lists.last_mut() {
                        *n += 1;
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    r.styles.pop();
                }
                TagEnd::Link => {
                    r.styles.pop();
                    if let Some(url) = r.link.take() {
                        let shown: String = r.spans.iter().map(|s| s.content.as_ref()).collect();
                        if !shown.ends_with(&url) && !url.starts_with('#') {
                            r.spans.push(Span::styled(format!(" ({url})"), theme::dim()));
                        }
                    }
                }
                TagEnd::TableCell => {
                    let c = std::mem::take(&mut r.cell);
                    if let Some(row) = r.table.as_mut().and_then(|t| t.last_mut()) {
                        row.push(c);
                    }
                }
                TagEnd::Table => {
                    if let Some(rows) = r.table.take() {
                        render_table(&rows, width, &mut r.out);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, code)) = r.code.as_mut() {
                    code.push_str(&t);
                } else if r.table.is_some() {
                    r.cell.push_str(&t);
                } else {
                    let st = r.style();
                    r.spans.push(Span::styled(t.to_string(), st));
                }
            }
            Event::Code(t) => {
                if r.table.is_some() {
                    r.cell.push_str(&t);
                } else {
                    r.spans.push(Span::styled(t.to_string(), theme::code()));
                }
            }
            Event::SoftBreak
                if r.table.is_none() => {
                    r.spans.push(Span::raw(" "));
                }
            Event::HardBreak => r.spans.push(Span::raw("\n")),
            Event::Rule => {
                r.flush();
                r.out.push(Line::from(Span::styled("─".repeat(width.min(60)), theme::dim())));
            }
            Event::TaskListMarker(done) => {
                r.spans.push(Span::styled(if done { "[x] " } else { "[ ] " }, theme::accent()));
            }
            Event::Html(t) | Event::InlineHtml(t) => r.spans.push(Span::styled(t.to_string(), theme::dim())),
            _ => {}
        }
    }
    r.flush();
    while r.out.last().is_some_and(|l| l.width() == 0) {
        r.out.pop();
    }
    r.out
}

fn render_table(rows: &[Vec<String>], width: usize, out: &mut Vec<Line<'static>>) {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let mut w = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.width());
        }
    }
    // Shrink widest columns until the table fits.
    let total = |w: &Vec<usize>| w.iter().sum::<usize>() + 3 * cols;
    while total(&w) > width {
        let (i, &m) = w.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
        if m <= 4 {
            break;
        }
        w[i] = m - 1;
    }
    for (ri, r) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        for i in 0..cols {
            let c = r.get(i).map(|s| s.as_str()).unwrap_or("");
            let mut text: String = String::new();
            let mut tw = 0;
            for ch in c.chars() {
                let cw = ch.width().unwrap_or(0);
                if tw + cw > w[i] {
                    break;
                }
                text.push(ch);
                tw += cw;
            }
            let pad = " ".repeat(w[i] - tw);
            let st = if ri == 0 { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
            spans.push(Span::styled(format!("{text}{pad}"), st));
            spans.push(Span::styled(if i + 1 < cols { " │ " } else { "" }, theme::dim()));
        }
        out.push(Line::from(spans));
        if ri == 0 {
            let sep: Vec<String> = w.iter().map(|n| "─".repeat(*n)).collect();
            out.push(Line::from(Span::styled(sep.join("─┼─"), theme::dim())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn renders_basics() {
        let out = plain(&render("# Title\n\nhello **world**\n\n- a\n- b\n\n```rs\nfn x() {}\n```", 40));
        assert_eq!(out[0], "Title");
        assert!(out.contains(&"hello world".to_string()));
        assert!(out.contains(&"• a".to_string()));
        assert!(out.contains(&"│ fn x() {}".to_string()));
    }

    #[test]
    fn wraps_words() {
        let out = plain(&wrap(vec![Span::raw("aaa bbb ccc")], 10, vec![], vec![Span::raw("  ")]));
        assert_eq!(out, vec!["aaa bbb", "  ccc"]);
    }
}
