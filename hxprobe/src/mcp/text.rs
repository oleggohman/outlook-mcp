//! Text helpers of the MCP server: HTML to plain text, length capping, text
//! from TEXT-or-BLOB columns, and ISO 8601 times.

use rusqlite::types::ValueRef;
use serde_json::Value;

use crate::inspect;

/// Elements dropped together with their content.
const DROPPED_ELEMENTS: [&str; 3] = ["script", "style", "head"];

/// Longest entity looked at after `&`, `;` included.
const MAX_ENTITY_LEN: usize = 32;

/// Largest accepted time: 9999-12-31T23:59:59Z.
const MAX_UNIX: i64 = 253_402_300_799;

/// What a tag turns into in the text.
fn tag_text(name: &str, closing: bool) -> Option<char> {
    match name {
        "br" => Some('\n'),
        "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if closing => {
            Some('\n')
        }
        // Keeps the words of adjacent table cells apart.
        "td" | "th" if closing => Some(' '),
        _ => None,
    }
}

/// True when `b` holds something that starts a tag: `<` followed by a
/// letter, `/`, `!` or `?`.
fn looks_like_html(b: &[u8]) -> bool {
    memchr::memchr_iter(b'<', b).any(|i| {
        matches!(b.get(i + 1), Some(c) if c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'))
    })
}

/// The character of the entity whose text between `&` and `;` is `name`.
fn entity(name: &str) -> Option<char> {
    let named = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let num = name.strip_prefix('#')?;
    let (digits, radix) = match num.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (num, 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let cp = u32::from_str_radix(digits, radix).ok()?;
    char::from_u32(cp).filter(|&c| c != '\0')
}

/// Plain text of an HTML body.
///
/// `<script>`, `<style>` and `<head>` elements and comments are dropped with
/// their content; `<br>` and the ends of `p`, `div`, `tr`, `li` and `h1`..`h6`
/// become line breaks, the ends of table cells a space; every other tag is
/// removed. The named entities `&amp; &lt; &gt; &quot; &apos; &nbsp;` and
/// numeric ones are decoded; anything else after `&` stays as it is. Runs of
/// spaces become one space and more than two line breaks become two; lines
/// are trimmed. In HTML, line breaks of the source are spaces; input that
/// holds no tag keeps its line breaks. An unclosed tag, comment or dropped
/// element ends the text.
///
/// Every search moves forward from the current position and the text after a
/// failed search is dropped, so the run time is linear in the input.
pub fn html_to_text(html: &str) -> String {
    let b = html.as_bytes();
    let is_html = looks_like_html(b);
    // ASCII lowering keeps byte offsets, so positions in `lower` are
    // positions in `html`.
    let lower = html.to_ascii_lowercase();
    let mut raw = String::with_capacity(html.len());
    let mut i = 0;
    while i < b.len() {
        let next = memchr::memchr2(b'<', b'&', &b[i..]).map_or(b.len(), |n| i + n);
        // Both markers are ASCII, so `i..next` lies on char boundaries.
        for c in html[i..next].chars() {
            raw.push(if is_html && c.is_whitespace() { ' ' } else { c });
        }
        i = next;
        if i >= b.len() {
            break;
        }
        if b[i] == b'&' {
            let window = &b[i + 1..b.len().min(i + 1 + MAX_ENTITY_LEN)];
            let decoded = memchr::memchr(b';', window).and_then(|n| {
                let name = &html[i + 1..i + 1 + n];
                entity(name).map(|c| (c, n + 2))
            });
            match decoded {
                Some((c, len)) => {
                    raw.push(c);
                    i += len;
                }
                None => {
                    raw.push('&');
                    i += 1;
                }
            }
            continue;
        }

        // `<`: a comment, a tag, or a literal `<`.
        if lower[i..].starts_with("<!--") {
            match lower[i + 4..].find("-->") {
                Some(n) => i += 4 + n + 3,
                None => break,
            }
            continue;
        }
        let after = b.get(i + 1).copied();
        let closing = after == Some(b'/');
        let name_at = i + 1 + usize::from(closing);
        let name_len = b[name_at..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        let starts_with_letter = b.get(name_at).is_some_and(u8::is_ascii_alphabetic);
        let is_tag = starts_with_letter || (!closing && matches!(after, Some(b'!' | b'?')));
        if !is_tag {
            raw.push('<');
            i += 1;
            continue;
        }
        let Some(end) = memchr::memchr(b'>', &b[i..]).map(|n| i + n + 1) else {
            break;
        };
        let name = &lower[name_at..name_at + name_len];
        i = end;
        if !closing && DROPPED_ELEMENTS.contains(&name) {
            let close = format!("</{name}");
            let Some(n) = lower[i..].find(&close) else {
                break;
            };
            let close_at = i + n;
            match memchr::memchr(b'>', &b[close_at..]) {
                Some(m) => i = close_at + m + 1,
                None => break,
            }
            continue;
        }
        if let Some(c) = tag_text(name, closing) {
            raw.push(c);
        }
    }
    collapse(&raw)
}

/// Collapse whitespace: `\r\n` and `\r` are line breaks, other whitespace
/// runs become one space, at most two line breaks in a row, no space at the
/// start or end of a line, nothing at the start or end of the text.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    let mut newlines = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let c = if c == '\r' {
            if chars.peek() == Some(&'\n') {
                continue;
            }
            '\n'
        } else {
            c
        };
        if c == '\n' {
            newlines += 1;
            space = false;
        } else if c.is_whitespace() {
            space = newlines == 0;
        } else {
            if !out.is_empty() {
                if newlines > 0 {
                    out.push_str(&"\n\n"[..newlines.min(2)]);
                } else if space {
                    out.push(' ');
                }
            }
            newlines = 0;
            space = false;
            out.push(c);
        }
    }
    out
}

/// `s` cut to at most `max_chars` chars, and whether anything was cut.
///
/// Control characters other than `\n` and `\t` are removed first and do not
/// count against the limit.
pub fn cap(s: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::with_capacity(s.len().min(max_chars.saturating_mul(4)));
    let mut n = 0;
    for c in s.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        if n == max_chars {
            return (out, true);
        }
        out.push(c);
        n += 1;
    }
    (out, false)
}

/// Text of a TEXT-or-BLOB column value: text as it is, a blob decoded as
/// UTF-8 with invalid bytes replaced, NULL as `None`. Numbers are formatted.
pub fn lossy(v: ValueRef) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Text(b) | ValueRef::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
    }
}

/// `YYYY-MM-DDThh:mm:ssZ` of unix seconds.
pub fn iso(unix: i64) -> String {
    inspect::utc_string(unix)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_date(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The number written by the ASCII digits `b`, or `None`.
fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0, |n, &c| n * 10 + u32::from(c - b'0')))
}

/// Unix seconds of `YYYY-MM-DD` or `YYYY-MM-DDThh:mm:ssZ`.
fn parse_iso(s: &str) -> Option<i64> {
    // Bytes, not chars: any non-ASCII byte fails the digit checks.
    let b = s.as_bytes();
    let time = match b.len() {
        10 => None,
        20 if b[10] == b'T' && b[13] == b':' && b[16] == b':' && b[19] == b'Z' => Some(&b[11..19]),
        _ => return None,
    };
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&b[..4])?;
    let m = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    let days = days_from_date(i64::from(y), m, d);
    // A day past the end of the month comes back as another date.
    if inspect::days_to_date(days) != (i64::from(y), m, d) {
        return None;
    }
    let secs = match time {
        None => 0,
        Some(t) => {
            let h = digits(&t[..2])?;
            let mi = digits(&t[3..5])?;
            let se = digits(&t[6..8])?;
            if h > 23 || mi > 59 || se > 59 {
                return None;
            }
            i64::from(h * 3600 + mi * 60 + se)
        }
    };
    Some(days * 86_400 + secs)
}

/// Unix seconds of a string of ASCII digits, or `None`.
fn parse_unix_digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Unix seconds of a time argument: an integer or a string of digits (unix
/// seconds), `YYYY-MM-DD` (midnight UTC) or `YYYY-MM-DDThh:mm:ssZ`, in the
/// years 1970 to 9999.
pub fn parse_time(v: &Value) -> Result<i64, String> {
    let t = match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => parse_unix_digits(s).or_else(|| parse_iso(s)),
        _ => None,
    }
    .filter(|t| (0..=MAX_UNIX).contains(t));
    t.ok_or_else(|| {
        "expected unix seconds or a UTC time YYYY-MM-DD or YYYY-MM-DDThh:mm:ssZ \
         in the years 1970 to 9999"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn tags_are_stripped() {
        assert_eq!(
            html_to_text("<p class=\"x\">Hello <b>world</b></p>"),
            "Hello world"
        );
        assert_eq!(html_to_text("<span>a</span><i>b</i>"), "ab");
        assert_eq!(
            html_to_text("<!DOCTYPE html><html><body>x</body></html>"),
            "x"
        );
    }

    #[test]
    fn script_style_head_and_comments_are_removed_with_their_content() {
        let html = "<html><head><title>T</title><style>p { color: red }</style></head>\
                    <body>a<script type=\"x\">var s = '<p>no</p>';</script>b\
                    <!-- hidden <p>comment</p> -->c<STYLE>q{}</STYLE>d</body></html>";
        assert_eq!(html_to_text(html), "abcd");
    }

    #[test]
    fn header_tag_is_not_head() {
        assert_eq!(html_to_text("<header>top</header>"), "top");
    }

    #[test]
    fn block_ends_become_newlines() {
        assert_eq!(html_to_text("a<br>b<br/>c<BR />d"), "a\nb\nc\nd");
        assert_eq!(
            html_to_text(
                "<p>one</p><div>two</div><h2>three</h2><ul><li>four</li><li>five</li></ul>"
            ),
            "one\ntwo\nthree\nfour\nfive"
        );
        assert_eq!(
            html_to_text("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>"),
            "a b\nc"
        );
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            html_to_text("a &amp; b &lt;c&gt; &quot;d&quot; &apos;e&apos;"),
            "a & b <c> \"d\" 'e'"
        );
        assert_eq!(html_to_text("x&nbsp;y"), "x y");
        assert_eq!(html_to_text("&#65;&#x42;&#X43;&#1513;"), "ABCש");
        // Decoded `<` does not start a tag.
        assert_eq!(html_to_text("&lt;b&gt;bold"), "<b>bold");
    }

    #[test]
    fn invalid_entities_stay_as_text() {
        assert_eq!(html_to_text("a &bogus; b"), "a &bogus; b");
        assert_eq!(html_to_text("AT&T and &amp"), "AT&T and &amp");
        assert_eq!(
            html_to_text("&#xZZ; &#; &#0; &#xD800; &#99999999999;"),
            "&#xZZ; &#; &#0; &#xD800; &#99999999999;"
        );
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(
            html_to_text("<p>  a \n\t b  </p>\n\n<p>c</p><br><br><br><br>d   "),
            "a b\nc\n\nd"
        );
        assert_eq!(html_to_text("   "), "");
        assert_eq!(html_to_text(""), "");
    }

    #[test]
    fn plain_text_keeps_its_line_breaks() {
        assert_eq!(
            html_to_text("Hi Ann,\r\n\r\n\r\n  see   you \tat 5 < 6.\nBob"),
            "Hi Ann,\n\nsee you at 5 < 6.\nBob"
        );
    }

    #[test]
    fn unclosed_tag_and_unclosed_script_end_the_text() {
        assert_eq!(html_to_text("<p>before</p><a href=\"x"), "before");
        assert_eq!(html_to_text("<p>before</p><script>var a = 1;"), "before");
        assert_eq!(html_to_text("<p>before</p><!-- open"), "before");
        assert_eq!(html_to_text("<"), "<");
        assert_eq!(html_to_text("a <"), "a <");
    }

    #[test]
    fn hebrew_text_survives() {
        assert_eq!(
            html_to_text("<div dir=\"rtl\">שלום&nbsp;<b>עולם</b></div>"),
            "שלום עולם"
        );
    }

    #[test]
    fn pathological_input_is_linear() {
        let lt = "<".repeat(1 << 20);
        let t = Instant::now();
        let out = html_to_text(&lt);
        assert_eq!(out.len(), 1 << 20);
        let open = "<a".repeat(1 << 19);
        assert_eq!(html_to_text(&open), "");
        let scripts = "<script>".repeat(1 << 17);
        assert_eq!(html_to_text(&scripts), "");
        let amps = "&#".repeat(1 << 19);
        assert_eq!(html_to_text(&amps), amps);
        assert!(t.elapsed() < Duration::from_secs(5), "too slow");
    }

    #[test]
    fn cap_cuts_on_char_boundaries() {
        assert_eq!(cap("שלום עולם", 4), ("שלום".to_string(), true));
        assert_eq!(cap("שלום", 4), ("שלום".to_string(), false));
        assert_eq!(cap("ab😀cd", 3), ("ab😀".to_string(), true));
        assert_eq!(cap("", 3), (String::new(), false));
        assert_eq!(cap("abc", 0), (String::new(), true));
    }

    #[test]
    fn cap_removes_control_characters_but_keeps_newline_and_tab() {
        assert_eq!(
            cap("a\u{0}b\u{7}c\r\nd\te\u{1b}[0m", 100),
            ("abc\nd\te[0m".to_string(), false)
        );
        // Removed characters do not count against the limit.
        assert_eq!(cap("\u{0}\u{0}abc", 3), ("abc".to_string(), false));
    }

    #[test]
    fn lossy_reads_text_and_blobs() {
        assert_eq!(
            lossy(ValueRef::Text("héllo".as_bytes())),
            Some("héllo".into())
        );
        assert_eq!(
            lossy(ValueRef::Blob("שלום".as_bytes())),
            Some("שלום".into())
        );
        assert_eq!(
            lossy(ValueRef::Blob(b"caf\xe9 ok")),
            Some("caf\u{fffd} ok".into())
        );
        assert_eq!(lossy(ValueRef::Null), None);
    }

    #[test]
    fn iso_and_parse_time_round_trip() {
        for (s, unix) in [
            ("1970-01-01T00:00:00Z", 0),
            ("2000-02-29T00:00:00Z", 951_782_400),
            ("2026-11-23T08:15:30Z", 1_795_421_730),
            ("9999-12-31T23:59:59Z", 253_402_300_799),
        ] {
            assert_eq!(iso(unix), s);
            assert_eq!(parse_time(&json!(s)), Ok(unix), "{s}");
        }
    }

    #[test]
    fn parse_time_accepts_dates_and_integers() {
        assert_eq!(parse_time(&json!("2000-02-29")), Ok(951_782_400));
        assert_eq!(parse_time(&json!("1970-01-01")), Ok(0));
        assert_eq!(parse_time(&json!(1_795_421_730)), Ok(1_795_421_730));
        assert_eq!(parse_time(&json!(0)), Ok(0));
        // A string of ASCII digits is unix seconds too.
        assert_eq!(parse_time(&json!("1795421730")), Ok(1_795_421_730));
        assert_eq!(parse_time(&json!("0")), Ok(0));
        assert_eq!(parse_time(&json!("253402300799")), Ok(253_402_300_799));
    }

    #[test]
    fn parse_time_rejects_bad_digit_strings() {
        for s in [
            "-1",
            "+5",
            "12a",
            " 5",
            "5 ",
            "1.5",
            "1e9",
            "253402300800",
            "99999999999999999999999",
            "１２",
        ] {
            assert!(parse_time(&json!(s)).is_err(), "{s}");
        }
    }

    #[test]
    fn parse_time_rejects_bad_values() {
        for v in [
            json!("2026-13-01"),
            json!("2026-02-30"),
            json!("2025-02-29"),
            json!("2026-00-10"),
            json!("2026-01-00"),
            json!("2026-01-01T24:00:00Z"),
            json!("2026-01-01T10:60:00Z"),
            json!("2026-01-01T10:00:60Z"),
            json!("2026-01-01T10:00:00"),
            json!("2026-01-01 10:00:00Z"),
            json!("1969-12-31"),
            json!("10000-01-01"),
            json!("+026-01-01"),
            json!("2026-1-01"),
            json!("２０２６-01-01"),
            json!(""),
            json!(true),
            json!(null),
            json!(1.5),
            json!(-1),
            json!(253_402_300_800i64),
            json!(["2026-01-01"]),
            json!({}),
        ] {
            assert!(parse_time(&v).is_err(), "{v}");
        }
    }
}
