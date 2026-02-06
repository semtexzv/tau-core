// ANSI escape code utilities and visible width calculation.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Extract an ANSI escape code starting at byte position `pos` in the string.
///
/// Returns `Some((code, len))` where `code` is the full escape sequence and
/// `len` is the number of bytes consumed. Returns `None` if no escape code
/// starts at the given position.
///
/// Handles:
/// - CSI sequences: `\x1b[...{final}` where final byte is 0x40–0x7E
/// - OSC sequences: `\x1b]...(\x07|\x1b\\)`
/// - APC sequences: `\x1b_...(\x07|\x1b\\)`
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if pos >= bytes.len() || bytes[pos] != ESC {
        return None;
    }

    // Need at least ESC + one more byte
    if pos + 1 >= bytes.len() {
        return None;
    }

    match bytes[pos + 1] {
        b'[' => extract_csi(bytes, pos),
        b']' => extract_string_sequence(bytes, pos),
        b'_' => extract_string_sequence(bytes, pos),
        _ => None,
    }
}

/// Extract a CSI sequence: `\x1b[` followed by parameter bytes (0x30-0x3F),
/// intermediate bytes (0x20-0x2F), and a final byte (0x40-0x7E).
fn extract_csi(bytes: &[u8], pos: usize) -> Option<(String, usize)> {
    let start = pos;
    let mut i = pos + 2; // skip ESC and [

    // Parameter bytes: 0x30–0x3F (digits, semicolons, etc.)
    while i < bytes.len() && (0x30..=0x3F).contains(&bytes[i]) {
        i += 1;
    }

    // Intermediate bytes: 0x20–0x2F
    while i < bytes.len() && (0x20..=0x2F).contains(&bytes[i]) {
        i += 1;
    }

    // Final byte: 0x40–0x7E
    if i < bytes.len() && (0x40..=0x7E).contains(&bytes[i]) {
        i += 1;
        let code = String::from_utf8_lossy(&bytes[start..i]).into_owned();
        Some((code, i - start))
    } else {
        None
    }
}

/// Extract an OSC (`\x1b]`) or APC (`\x1b_`) sequence, terminated by BEL
/// (`\x07`) or ST (`\x1b\\`).
fn extract_string_sequence(bytes: &[u8], pos: usize) -> Option<(String, usize)> {
    let start = pos;
    let mut i = pos + 2; // skip ESC and ] or _

    while i < bytes.len() {
        if bytes[i] == BEL {
            i += 1; // consume BEL
            let code = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            return Some((code, i - start));
        }
        if bytes[i] == ESC && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            i += 2; // consume ESC and backslash
            let code = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            return Some((code, i - start));
        }
        i += 1;
    }

    None // unterminated sequence
}

/// Remove all ANSI escape sequences from a string.
///
/// Strips CSI (`\x1b[...`), OSC (`\x1b]...\x07`), and APC (`\x1b_...\x07`)
/// sequences, returning only the visible text.
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == ESC {
            if let Some((_, len)) = extract_ansi_code(s, i) {
                i += len;
                continue;
            }
        }
        // Safe because we're walking byte-by-byte through valid UTF-8.
        // If this byte starts a multi-byte char, we need to grab the full char.
        if let Some(ch) = s[i..].chars().next() {
            result.push(ch);
            i += ch.len_utf8();
        } else {
            i += 1;
        }
    }

    result
}

/// Calculate the visible width of a string in terminal columns.
///
/// Strips ANSI escape codes, then measures using Unicode width rules.
/// Tabs are counted as 3 spaces (matching pi-mono convention).
pub fn visible_width(s: &str) -> usize {
    let stripped = strip_ansi(s);
    stripped
        .chars()
        .map(|c| {
            if c == '\t' {
                3
            } else {
                // UnicodeWidthStr works on &str slices; use char-level width
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                UnicodeWidthStr::width(s)
            }
        })
        .sum()
}

/// Truncate a string to fit within `max_width` visible columns, appending
/// `ellipsis` if the string was truncated.
///
/// Preserves ANSI escape codes (they don't count toward width).
/// Grapheme-cluster-aware: never splits a multi-byte character.
///
/// If `max_width` is smaller than the ellipsis width, returns a truncation
/// to exactly `max_width` columns with no ellipsis.
pub fn truncate_to_width(s: &str, max_width: usize, ellipsis: &str) -> String {
    let full_visible = visible_width(s);
    if full_visible <= max_width {
        return s.to_string();
    }

    let ellipsis_width = visible_width(ellipsis);
    let content_budget = max_width.saturating_sub(ellipsis_width);

    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len());
    let mut current_width: usize = 0;
    let mut i = 0;

    while i < bytes.len() {
        // Pass through ANSI escape codes without counting width
        if bytes[i] == ESC {
            if let Some((code, len)) = extract_ansi_code(s, i) {
                result.push_str(&code);
                i += len;
                continue;
            }
        }

        // Get the next grapheme cluster starting at byte position i
        let remaining = &s[i..];
        if let Some(grapheme) = remaining.graphemes(true).next() {
            let gw = if grapheme == "\t" {
                3
            } else {
                UnicodeWidthStr::width(grapheme)
            };

            if current_width + gw > content_budget {
                break;
            }
            result.push_str(grapheme);
            current_width += gw;
            i += grapheme.len();
        } else {
            break;
        }
    }

    // Append ellipsis if we actually truncated and there's room for it
    if ellipsis_width <= max_width {
        result.push_str(ellipsis);
    }

    result
}

/// Check if a CSI code is an SGR (Select Graphic Rendition) code.
/// SGR codes start with `\x1b[` and end with `m`.
pub(crate) fn is_sgr(code: &str) -> bool {
    code.starts_with("\x1b[") && code.ends_with('m')
}

/// Update tracked SGR state. On `\x1b[0m` or `\x1b[m`, clears all state.
/// On any other SGR, appends it. Non-SGR codes are ignored.
pub(crate) fn update_sgr_state(state: &mut Vec<String>, code: &str) {
    if !is_sgr(code) {
        return;
    }
    if code == "\x1b[0m" || code == "\x1b[m" {
        state.clear();
    } else {
        state.push(code.to_string());
    }
}

/// Create an SGR prefix by concatenating all tracked SGR codes.
pub(crate) fn sgr_prefix(state: &[String]) -> String {
    state.concat()
}

/// Skip the first `skip` visible columns of a string, returning the remainder
/// along with the active SGR state at that point.
///
/// Returns `(sgr_prefix, remaining_content)` where `sgr_prefix` contains
/// all active ANSI SGR codes at the skip point (for re-application).
pub fn slice_from_column(s: &str, skip: usize) -> (String, String) {
    let bytes = s.as_bytes();
    let mut pos = 0;
    let mut col = 0;
    let mut sgr_state: Vec<String> = Vec::new();

    while pos < bytes.len() && col < skip {
        if bytes[pos] == ESC {
            if let Some((code, len)) = extract_ansi_code(s, pos) {
                update_sgr_state(&mut sgr_state, &code);
                pos += len;
                continue;
            }
        }

        let remaining = &s[pos..];
        if let Some(grapheme) = remaining.graphemes(true).next() {
            let w = if grapheme == "\t" { 3 } else { UnicodeWidthStr::width(grapheme) };
            col += w;
            pos += grapheme.len();
        } else {
            break;
        }
    }

    // Also consume any ANSI codes right at the skip boundary
    while pos < bytes.len() && bytes[pos] == ESC {
        if let Some((code, len)) = extract_ansi_code(s, pos) {
            update_sgr_state(&mut sgr_state, &code);
            pos += len;
        } else {
            break;
        }
    }

    (sgr_prefix(&sgr_state), s[pos..].to_string())
}

/// Word-wrap text to fit within `width` visible columns, preserving ANSI codes.
///
/// - Splits on word boundaries (spaces)
/// - Breaks words longer than `width` character-by-character (grapheme-aware)
/// - Tracks ANSI SGR state and re-applies at the start of each wrapped line
/// - Hard line breaks (`\n`) are preserved
///
/// Returns empty `Vec` for empty input or zero width.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() || width == 0 {
        return vec![];
    }

    let mut result = Vec::new();
    let mut sgr_state: Vec<String> = Vec::new();

    for hard_line in text.split('\n') {
        wrap_single_line(hard_line, width, &mut sgr_state, &mut result);
    }

    result
}

/// Wrap a single line (no newlines) into one or more output lines.
fn wrap_single_line(
    text: &str,
    width: usize,
    sgr_state: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    let bytes = text.as_bytes();
    let mut i = 0;

    let mut current_line = sgr_prefix(sgr_state);
    let mut current_width: usize = 0;

    // Break point tracking: byte position of last space in current_line
    let mut break_pos: Option<usize> = None;
    let mut break_width: usize = 0;
    let mut break_sgr: Vec<String> = sgr_state.clone();

    let initial_len = out.len();

    while i < bytes.len() {
        // Pass through ANSI codes
        if bytes[i] == ESC {
            if let Some((code, len)) = extract_ansi_code(text, i) {
                update_sgr_state(sgr_state, &code);
                current_line.push_str(&code);
                i += len;
                continue;
            }
        }

        // Get next grapheme
        let remaining = &text[i..];
        let grapheme = match remaining.graphemes(true).next() {
            Some(g) => g,
            None => {
                i += 1;
                continue;
            }
        };
        let gw = if grapheme == "\t" {
            3
        } else {
            UnicodeWidthStr::width(grapheme)
        };
        let grapheme_bytes = grapheme.len();

        if grapheme == " " {
            if current_width + 1 > width && current_width > 0 {
                // Space doesn't fit — break here without the space
                out.push(current_line);
                current_line = sgr_prefix(sgr_state);
                current_width = 0;
                break_pos = None;
            } else {
                // Record as potential break point
                break_pos = Some(current_line.len());
                break_width = current_width;
                break_sgr = sgr_state.clone();
                current_line.push(' ');
                current_width += 1;
            }
            i += grapheme_bytes;
            continue;
        }

        // Non-space visible character
        if current_width + gw > width {
            if let Some(bp) = break_pos {
                // Break at last space
                let after_break = current_line[bp + 1..].to_string();
                current_line.truncate(bp);
                out.push(current_line);

                let prefix = sgr_prefix(&break_sgr);
                current_line = format!("{}{}", prefix, after_break);
                current_width = current_width - break_width - 1;
                break_pos = None;
            } else if current_width == 0 {
                // Single grapheme wider than width — put it alone
                current_line.push_str(grapheme);
                out.push(current_line);
                current_line = sgr_prefix(sgr_state);
                i += grapheme_bytes;
                continue;
            } else {
                // No break point — mid-word break
                out.push(current_line);
                current_line = sgr_prefix(sgr_state);
                current_width = 0;
                break_pos = None;
            }
        }

        current_line.push_str(grapheme);
        current_width += gw;
        i += grapheme_bytes;
    }

    // Push final line if it has content, or if nothing was pushed (empty hard line)
    if current_width > 0 || out.len() == initial_len {
        out.push(current_line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_ansi ──────────────────────────────────────────────────

    #[test]
    fn strip_ansi_plain_text_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_removes_sgr_color() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
    }

    #[test]
    fn strip_ansi_removes_complex_sgr() {
        // Bold + underline + 256-color foreground
        assert_eq!(strip_ansi("\x1b[1;4;38;5;196mtext\x1b[0m"), "text");
    }

    #[test]
    fn strip_ansi_removes_cursor_movement() {
        // Cursor to column 5, clear line
        assert_eq!(strip_ansi("\x1b[5G\x1b[2Khi"), "hi");
    }

    #[test]
    fn strip_ansi_strips_osc_hyperlink() {
        let input = "\x1b]8;;https://example.com\x07click here\x1b]8;;\x07";
        assert_eq!(strip_ansi(input), "click here");
    }

    #[test]
    fn strip_ansi_strips_osc_with_st_terminator() {
        // OSC terminated with ESC \ instead of BEL
        let input = "\x1b]0;window title\x1b\\visible";
        assert_eq!(strip_ansi(input), "visible");
    }

    #[test]
    fn strip_ansi_strips_apc() {
        let input = "\x1b_some application data\x07visible";
        assert_eq!(strip_ansi(input), "visible");
    }

    #[test]
    fn strip_ansi_preserves_unicode() {
        assert_eq!(strip_ansi("\x1b[31m你好\x1b[0m"), "你好");
    }

    #[test]
    fn strip_ansi_multiple_sequences() {
        let input = "\x1b[1mbold\x1b[0m and \x1b[4munderline\x1b[0m";
        assert_eq!(strip_ansi(input), "bold and underline");
    }

    // ── extract_ansi_code ───────────────────────────────────────────

    #[test]
    fn extract_ansi_code_returns_none_for_non_escape() {
        assert_eq!(extract_ansi_code("hello", 0), None);
    }

    #[test]
    fn extract_ansi_code_returns_none_for_out_of_bounds() {
        assert_eq!(extract_ansi_code("hi", 10), None);
    }

    #[test]
    fn extract_ansi_code_returns_none_in_middle_of_text() {
        assert_eq!(extract_ansi_code("abc", 1), None);
    }

    #[test]
    fn extract_ansi_code_extracts_sgr() {
        let s = "\x1b[31m";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b[31m".to_string(), 5)));
    }

    #[test]
    fn extract_ansi_code_extracts_sgr_reset() {
        let s = "\x1b[0m";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b[0m".to_string(), 4)));
    }

    #[test]
    fn extract_ansi_code_extracts_complex_sgr() {
        let s = "\x1b[38;2;255;128;0m";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b[38;2;255;128;0m".to_string(), 17)));
    }

    #[test]
    fn extract_ansi_code_extracts_cursor_movement() {
        let s = "\x1b[10A"; // cursor up 10
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b[10A".to_string(), 5)));
    }

    #[test]
    fn extract_ansi_code_extracts_clear_line() {
        let s = "\x1b[2K";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b[2K".to_string(), 4)));
    }

    #[test]
    fn extract_ansi_code_at_offset() {
        let s = "hi\x1b[31mred";
        let result = extract_ansi_code(s, 2);
        assert_eq!(result, Some(("\x1b[31m".to_string(), 5)));
    }

    #[test]
    fn extract_ansi_code_osc_with_bel() {
        let s = "\x1b]8;;https://example.com\x07";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b]8;;https://example.com\x07".to_string(), s.len())));
    }

    #[test]
    fn extract_ansi_code_osc_with_st() {
        let s = "\x1b]0;title\x1b\\";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b]0;title\x1b\\".to_string(), s.len())));
    }

    #[test]
    fn extract_ansi_code_apc() {
        let s = "\x1b_data\x07";
        let result = extract_ansi_code(s, 0);
        assert_eq!(result, Some(("\x1b_data\x07".to_string(), 7)));
    }

    #[test]
    fn extract_ansi_code_unterminated_returns_none() {
        // CSI without a final byte
        assert_eq!(extract_ansi_code("\x1b[31", 0), None);
        // OSC without terminator
        assert_eq!(extract_ansi_code("\x1b]8;;url", 0), None);
    }

    #[test]
    fn extract_ansi_code_bare_esc_returns_none() {
        // Just ESC at end of string
        assert_eq!(extract_ansi_code("\x1b", 0), None);
    }

    // ── visible_width ───────────────────────────────────────────────

    #[test]
    fn visible_width_ascii() {
        assert_eq!(visible_width("hello"), 5);
    }

    #[test]
    fn visible_width_empty() {
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn visible_width_ignores_ansi() {
        assert_eq!(visible_width("\x1b[31mhello\x1b[0m"), 5);
    }

    #[test]
    fn visible_width_complex_ansi() {
        assert_eq!(visible_width("\x1b[1;4;38;5;196mtext\x1b[0m"), 4);
    }

    #[test]
    fn visible_width_wide_chars() {
        assert_eq!(visible_width("你好"), 4);
    }

    #[test]
    fn visible_width_mixed_wide_and_ascii() {
        assert_eq!(visible_width("hi你好"), 6); // 2 + 4
    }

    #[test]
    fn visible_width_tab_counts_as_3() {
        assert_eq!(visible_width("\t"), 3);
    }

    #[test]
    fn visible_width_tabs_in_text() {
        assert_eq!(visible_width("a\tb"), 5); // 1 + 3 + 1
    }

    #[test]
    fn visible_width_osc_hyperlink() {
        let input = "\x1b]8;;https://example.com\x07click\x1b]8;;\x07";
        assert_eq!(visible_width(input), 5);
    }

    // ── truncate_to_width ───────────────────────────────────────────

    #[test]
    fn truncate_no_truncation_needed() {
        assert_eq!(truncate_to_width("hello", 10, "..."), "hello");
    }

    #[test]
    fn truncate_basic() {
        assert_eq!(truncate_to_width("hello world", 8, "..."), "hello...");
    }

    #[test]
    fn truncate_exact_fit() {
        assert_eq!(truncate_to_width("hello", 5, "..."), "hello");
    }

    #[test]
    fn truncate_preserves_ansi() {
        let input = "\x1b[31mhello world\x1b[0m";
        let result = truncate_to_width(input, 8, "...");
        // Should keep the color code, truncate visible text, add ellipsis
        assert_eq!(result, "\x1b[31mhello...");
        // Visible width should be 8
        assert_eq!(visible_width(&result), 8);
    }

    #[test]
    fn truncate_wide_chars() {
        // "你好世界" = 8 columns, truncate to 6 with "..."
        // budget = 6 - 3 = 3 columns, only "你" fits (2 cols), "好" would be 4
        let result = truncate_to_width("你好世界", 6, "...");
        assert_eq!(result, "你...");
        assert_eq!(visible_width(&result), 5); // 2 + 3
    }

    #[test]
    fn truncate_max_width_smaller_than_ellipsis() {
        // max_width=2, ellipsis="..." (3 chars) — content budget saturates to 0
        let result = truncate_to_width("hello", 2, "...");
        // content_budget = 0, nothing fits, but ellipsis_width > max_width
        // so no ellipsis either — empty result
        assert_eq!(visible_width(&result), 0);
    }

    #[test]
    fn truncate_empty_ellipsis() {
        let result = truncate_to_width("hello world", 5, "");
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_with_ansi_in_middle() {
        let input = "he\x1b[31mllo world\x1b[0m";
        let result = truncate_to_width(input, 8, "...");
        assert_eq!(result, "he\x1b[31mllo...");
        assert_eq!(visible_width(&result), 8);
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_to_width("", 5, "..."), "");
    }

    // ── wrap_text_with_ansi ─────────────────────────────────────────

    #[test]
    fn wrap_empty_text() {
        assert_eq!(wrap_text_with_ansi("", 80), Vec::<String>::new());
    }

    #[test]
    fn wrap_zero_width() {
        assert_eq!(wrap_text_with_ansi("hello", 0), Vec::<String>::new());
    }

    #[test]
    fn wrap_short_text_no_wrap() {
        assert_eq!(wrap_text_with_ansi("hello", 80), vec!["hello"]);
    }

    #[test]
    fn wrap_at_word_boundary() {
        assert_eq!(
            wrap_text_with_ansi("hello world", 7),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn wrap_exact_fit_no_wrap() {
        assert_eq!(
            wrap_text_with_ansi("hello", 5),
            vec!["hello"]
        );
    }

    #[test]
    fn wrap_multiple_words() {
        assert_eq!(
            wrap_text_with_ansi("the quick brown fox", 10),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn wrap_long_word_char_by_char() {
        assert_eq!(
            wrap_text_with_ansi("abcdefgh", 5),
            vec!["abcde", "fgh"]
        );
    }

    #[test]
    fn wrap_long_word_multiple_breaks() {
        assert_eq!(
            wrap_text_with_ansi("abcdefghi", 3),
            vec!["abc", "def", "ghi"]
        );
    }

    #[test]
    fn wrap_ansi_style_preserved_across_lines() {
        let input = "\x1b[31mhello world\x1b[0m";
        let result = wrap_text_with_ansi(input, 7);
        // Line 1: red "hello", line 2: red carried over + "world" + reset
        assert_eq!(result, vec!["\x1b[31mhello", "\x1b[31mworld\x1b[0m"]);
    }

    #[test]
    fn wrap_ansi_style_changes_mid_text() {
        let input = "\x1b[31mhello \x1b[32mworld\x1b[0m";
        let result = wrap_text_with_ansi(input, 7);
        // Line 1: red "hello ", line 2 gets red prefix + green override + "world" + reset
        assert_eq!(
            result,
            vec!["\x1b[31mhello", "\x1b[31m\x1b[32mworld\x1b[0m"]
        );
    }

    #[test]
    fn wrap_preserves_hard_newlines() {
        assert_eq!(
            wrap_text_with_ansi("hello\nworld", 80),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn wrap_hard_newline_with_empty_line() {
        assert_eq!(
            wrap_text_with_ansi("hello\n\nworld", 80),
            vec!["hello", "", "world"]
        );
    }

    #[test]
    fn wrap_wide_chars() {
        // "你好世界" = 8 visible columns, wrap at 5
        assert_eq!(
            wrap_text_with_ansi("你好世界", 5),
            vec!["你好", "世界"]
        );
    }

    #[test]
    fn wrap_wide_char_doesnt_split() {
        // Width 3: "你" (2) fits, "好" (2) would make 4 > 3, so break
        assert_eq!(
            wrap_text_with_ansi("你好", 3),
            vec!["你", "好"]
        );
    }

    #[test]
    fn wrap_sgr_state_across_hard_newlines() {
        let input = "\x1b[31mhello\nworld\x1b[0m";
        let result = wrap_text_with_ansi(input, 80);
        // SGR state carries across hard newlines
        assert_eq!(result, vec!["\x1b[31mhello", "\x1b[31mworld\x1b[0m"]);
    }

    #[test]
    fn wrap_word_with_trailing_space_at_width() {
        // "hello world" at width 5: 'hello' fills, space overflows
        assert_eq!(
            wrap_text_with_ansi("hello world", 5),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn wrap_single_wide_char_wider_than_width() {
        // Width 1, "你" (width 2) — single char wider than allowed
        let result = wrap_text_with_ansi("你", 1);
        assert_eq!(result, vec!["你"]);
    }

    // ── slice_from_column ───────────────────────────────────────────

    #[test]
    fn slice_from_column_plain_text() {
        let (sgr, rest) = slice_from_column("hello world", 6);
        assert_eq!(sgr, "");
        assert_eq!(rest, "world");
    }

    #[test]
    fn slice_from_column_skip_zero() {
        let (sgr, rest) = slice_from_column("hello", 0);
        assert_eq!(sgr, "");
        assert_eq!(rest, "hello");
    }

    #[test]
    fn slice_from_column_skip_all() {
        let (sgr, rest) = slice_from_column("hello", 5);
        assert_eq!(sgr, "");
        assert_eq!(rest, "");
    }

    #[test]
    fn slice_from_column_skip_past_end() {
        let (sgr, rest) = slice_from_column("hi", 10);
        assert_eq!(sgr, "");
        assert_eq!(rest, "");
    }

    #[test]
    fn slice_from_column_with_ansi() {
        // "\x1b[31mhello\x1b[0m world" — skip 5 (past "hello")
        let (sgr, rest) = slice_from_column("\x1b[31mhello\x1b[0m world", 5);
        // The \x1b[31m was active, then \x1b[0m reset it at the skip boundary
        assert_eq!(sgr, "");
        assert_eq!(rest, " world");
    }

    #[test]
    fn slice_from_column_preserves_active_sgr() {
        // "\x1b[31mhello world\x1b[0m" — skip 6 (past "hello ")
        // SGR state at skip point: \x1b[31m is still active
        let (sgr, rest) = slice_from_column("\x1b[31mhello world\x1b[0m", 6);
        assert_eq!(sgr, "\x1b[31m");
        assert_eq!(rest, "world\x1b[0m");
    }

    #[test]
    fn slice_from_column_wide_chars() {
        // "你好世界" = 8 columns, skip 4 (past "你好")
        let (sgr, rest) = slice_from_column("你好世界", 4);
        assert_eq!(sgr, "");
        assert_eq!(rest, "世界");
    }

    #[test]
    fn slice_from_column_empty_string() {
        let (sgr, rest) = slice_from_column("", 5);
        assert_eq!(sgr, "");
        assert_eq!(rest, "");
    }
}
