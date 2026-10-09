use crate::herdr::client::HerdrClient;
use crate::model::{MatchSpan, PaneId, VisibleViewport};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnderlinedSpan {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub text: String,
}

/// Parses multi-line ANSI terminal output and extracts all spans with underline style (SGR 4).
pub fn parse_underlined_spans(ansi_text: &str) -> Vec<UnderlinedSpan> {
    let mut spans = Vec::new();
    for (row_idx, line) in ansi_text.lines().enumerate() {
        parse_row_underlined_spans(row_idx, line, &mut spans);
    }
    spans
}

fn parse_row_underlined_spans(row_idx: usize, line: &str, spans: &mut Vec<UnderlinedSpan>) {
    let mut chars = line.chars().peekable();
    let mut col = 0;
    let mut is_underlined = false;
    let mut current_start_col = 0;
    let mut current_text = String::new();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next(); // consume '['
                    let mut params = String::new();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if (0x40..=0x7e).contains(&(c as u32)) {
                            if c == 'm' {
                                handle_sgr(
                                    &params,
                                    &mut is_underlined,
                                    row_idx,
                                    col,
                                    &mut current_start_col,
                                    &mut current_text,
                                    spans,
                                );
                            }
                            break;
                        } else {
                            params.push(c);
                        }
                    }
                }
                Some(&']') => {
                    // OSC sequence: consume until ST (\x1b\ or \x07)
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    // Other escape sequence: consume next char
                    chars.next();
                }
                None => {}
            }
        } else {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if is_underlined {
                current_text.push(ch);
            }
            col += width;
        }
    }

    flush_span(
        &mut is_underlined,
        row_idx,
        col,
        current_start_col,
        &mut current_text,
        spans,
    );
}

fn handle_sgr(
    params: &str,
    is_underlined: &mut bool,
    row_idx: usize,
    current_col: usize,
    current_start_col: &mut usize,
    current_text: &mut String,
    spans: &mut Vec<UnderlinedSpan>,
) {
    if params.is_empty() {
        flush_span(
            is_underlined,
            row_idx,
            current_col,
            *current_start_col,
            current_text,
            spans,
        );
        return;
    }

    for part in params.split(';') {
        let code = part.split(':').next().unwrap_or(part);
        match code {
            "0" | "24" => {
                flush_span(
                    is_underlined,
                    row_idx,
                    current_col,
                    *current_start_col,
                    current_text,
                    spans,
                );
            }
            "4" => {
                if !*is_underlined {
                    *is_underlined = true;
                    *current_start_col = current_col;
                    current_text.clear();
                }
            }
            _ => {}
        }
    }
}

fn flush_span(
    is_underlined: &mut bool,
    row_idx: usize,
    current_col: usize,
    current_start_col: usize,
    current_text: &mut String,
    spans: &mut Vec<UnderlinedSpan>,
) {
    if *is_underlined {
        if !current_text.trim().is_empty() {
            spans.push(UnderlinedSpan {
                row: row_idx,
                col_start: current_start_col,
                col_end: current_col,
                text: std::mem::take(current_text),
            });
        } else {
            current_text.clear();
        }
        *is_underlined = false;
    }
}

/// Converts a 0-based display column to a byte offset in `text`.
pub fn col_to_byte_offset(text: &str, target_col: usize) -> usize {
    let mut current_col = 0;
    for (byte_idx, ch) in text.char_indices() {
        if current_col >= target_col {
            return byte_idx;
        }
        current_col += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    text.len()
}

/// Maps an `UnderlinedSpan` and target URL to a `MatchSpan` on the logical line.
pub fn span_to_match_span(
    span: &UnderlinedSpan,
    url: String,
    viewport: &VisibleViewport,
) -> Option<MatchSpan> {
    let segment = viewport.segments.iter().find(|s| s.row == span.row)?;
    let row_text = viewport.rows.get(span.row)?;
    let byte_start = col_to_byte_offset(row_text, span.col_start);
    let byte_end = col_to_byte_offset(row_text, span.col_end);
    if byte_start >= byte_end {
        return None;
    }

    let logical_start = segment.logical_start + byte_start;
    let logical_end = segment.logical_start + byte_end;

    Some(MatchSpan {
        line: segment.logical_line,
        start: logical_start,
        end: logical_end,
        text: url,
        pattern: "hyperlink".to_string(),
        priority: 5,
    })
}

/// Merges contiguous spans on the same logical line with the same target URL.
pub fn merge_hyperlink_spans(spans: Vec<MatchSpan>) -> Vec<MatchSpan> {
    let mut merged: Vec<MatchSpan> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut() {
            if last.line == span.line && last.text == span.text && last.end == span.start {
                last.end = span.end;
                continue;
            }
        }
        merged.push(span);
    }
    merged
}

/// Queries Herdr for each underlined span's link target and builds `MatchSpan`s.
pub fn detect_hyperlinks<C: HerdrClient>(
    client: &mut C,
    target: &PaneId,
    ansi_text: &str,
    viewport: &VisibleViewport,
) -> Vec<MatchSpan> {
    let spans = parse_underlined_spans(ansi_text);
    if spans.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for span in spans {
        let col_to_query = span.col_start as u16;
        let row_to_query = span.row as u16;
        if let Ok(Some(url)) = client.pane_link_activate(target, row_to_query, col_to_query) {
            if let Some(match_span) = span_to_match_span(&span, url, viewport) {
                matches.push(match_span);
            }
        }
    }

    merge_hyperlink_spans(matches)
}

/// Merges candidate matches, giving priority to hyperlinks and resolving overlaps.
pub fn merge_matches(
    regex_matches: Vec<MatchSpan>,
    extra_matches: Vec<MatchSpan>,
) -> Vec<MatchSpan> {
    if extra_matches.is_empty() {
        return regex_matches;
    }

    let mut all = extra_matches;
    all.extend(regex_matches);
    all.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.len_bytes().cmp(&a.len_bytes()))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.start.cmp(&b.start))
    });

    let mut accepted: Vec<MatchSpan> = Vec::new();
    for span in all {
        if accepted.iter().all(|existing| !span.overlaps(existing)) {
            accepted.push(span);
        }
    }

    accepted.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then_with(|| a.start.cmp(&b.start))
            .then_with(|| a.end.cmp(&b.end))
    });

    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewport::map_visible_viewport;

    #[test]
    fn parses_live_herdr_ansi_line() {
        let ansi = "\x1b[0m\x1b[38;2;255;255;255m\x1b[48;2;30;30;46m     \x1b[0m\x1b[38;2;137;180;250m\x1b[48;2;30;30;46m- \x1b[0m\x1b[1m\x1b[4m\x1b[38;2;137;220;235m\x1b[48;2;30;30;46m#44092: core(v2): \"unknown finish reason\" error omits raw provider finish_reason\x1b[0m\x1b[38;2;203;166;247m\x1b[48;2;30;30;46m  \x1b[0m";
        let spans = parse_underlined_spans(ansi);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].row, 0);
        assert_eq!(spans[0].col_start, 7);
        assert_eq!(
            spans[0].text,
            "#44092: core(v2): \"unknown finish reason\" error omits raw provider finish_reason"
        );
    }

    #[test]
    fn parses_simple_underlined_text() {
        let ansi = "hello \x1b[4mworld\x1b[0m test";
        let spans = parse_underlined_spans(ansi);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].row, 0);
        assert_eq!(spans[0].col_start, 6);
        assert_eq!(spans[0].col_end, 11);
        assert_eq!(spans[0].text, "world");
    }

    #[test]
    fn parses_combined_sgr_parameters() {
        let ansi = "prefix \x1b[1m\x1b[4m\x1b[38;2;137;220;235m#44092: issue title\x1b[0m suffix";
        let spans = parse_underlined_spans(ansi);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].row, 0);
        assert_eq!(spans[0].col_start, 7);
        assert_eq!(spans[0].col_end, 26);
        assert_eq!(spans[0].text, "#44092: issue title");
    }

    #[test]
    fn maps_span_to_match_span_with_url() {
        let raw = "prefix #44092: issue title suffix";
        let viewport = map_visible_viewport(vec![raw.to_string()], 80, 1);
        let span = UnderlinedSpan {
            row: 0,
            col_start: 7,
            col_end: 27,
            text: "#44092: issue title".to_string(),
        };
        let match_span = span_to_match_span(
            &span,
            "https://github.com/anomalyco/opencode/issues/44092".to_string(),
            &viewport,
        )
        .unwrap();

        assert_eq!(match_span.line, 0);
        assert_eq!(match_span.start, 7);
        assert_eq!(match_span.end, 27);
        assert_eq!(
            match_span.text,
            "https://github.com/anomalyco/opencode/issues/44092"
        );
        assert_eq!(match_span.priority, 5);
    }

    #[test]
    fn merge_matches_prefers_hyperlink_over_overlapping_regex() {
        let hyperlink = MatchSpan {
            line: 0,
            start: 7,
            end: 27,
            text: "https://github.com/anomalyco/opencode/issues/44092".to_string(),
            pattern: "hyperlink".to_string(),
            priority: 5,
        };
        let number_regex = MatchSpan {
            line: 0,
            start: 8,
            end: 13,
            text: "44092".to_string(),
            pattern: "number".to_string(),
            priority: 60,
        };
        let merged = merge_matches(vec![number_regex], vec![hyperlink.clone()]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], hyperlink);
    }
}
