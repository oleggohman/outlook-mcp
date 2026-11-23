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

/// Most `<` inside the quoted attribute values of one tag; a tag with more
/// is text.
const MAX_QUOTED_LT: usize = 8;

/// A tag found by [`tag_at`].
struct Tag {
    /// Where the name starts and how long it is (ASCII letters and digits;
    /// empty for `<!...>` and `<?...>`).
    name_at: usize,
    name_len: usize,
    closing: bool,
    /// Index after the closing `>`.
    end: usize,
}

/// Where the name of the tag opened by the `<` at `b[i]` starts and whether
/// it is a closing tag, or `None` when that `<` cannot open a tag. A tag
/// opens with `<` followed by a letter, `/` and a letter, `!` or `?`. `<`
/// followed by an address (`<ann@example.org>`: an `@` before any space,
/// quote, `=`, `/`, `<` or `>`) or by a URL (`<https://example.org>`: a
/// scheme and `://`) opens no tag.
fn tag_open(b: &[u8], i: usize) -> Option<(usize, bool)> {
    let after = b.get(i + 1).copied();
    let closing = after == Some(b'/');
    let name_at = i + 1 + usize::from(closing);
    if !b.get(name_at).is_some_and(u8::is_ascii_alphabetic) {
        return (!closing && matches!(after, Some(b'!' | b'?'))).then_some((name_at, false));
    }
    let rest = &b[name_at..];
    let is_address = rest
        .iter()
        .take_while(|c| {
            !c.is_ascii_whitespace() && !matches!(c, b'"' | b'\'' | b'=' | b'/' | b'<' | b'>')
        })
        .any(|&c| c == b'@');
    let scheme_len = rest
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        .count();
    if is_address || rest[scheme_len..].starts_with(b"://") {
        return None;
    }
    Some((name_at, closing))
}

/// Index of the `>` that closes the tag whose inside starts at `b[from]`,
/// or `None` when a `<` or the end of the input comes first.
///
/// A quote that follows `=` (spaces allowed between them) opens an
/// attribute value that runs to the next quote of the same kind; `<` and
/// `>` inside it do not count. A quote with no closing quote is an ordinary
/// character. A tag whose values hold more than [`MAX_QUOTED_LT`] `<` is not
/// a tag.
///
/// The search stops at the first `<` outside a value and passes at most
/// [`MAX_QUOTED_LT`] `<` inside values, so calling it for every `<` of a
/// text is linear in the text.
fn tag_close(b: &[u8], from: usize) -> Option<usize> {
    let mut quoted_lt = 0;
    let mut p = from;
    // The first `<` or `>` at or after `p`, found once per stretch.
    let mut stop = None;
    loop {
        let end = match stop {
            Some(end) if end >= p => end,
            _ => p + memchr::memchr2(b'<', b'>', &b[p..])?,
        };
        stop = Some(end);
        let Some(n) = memchr::memchr2(b'"', b'\'', &b[p..end]) else {
            return (b[end] == b'>').then_some(end);
        };
        let at = p + n;
        p = at + 1;
        let follows_eq = b[from..at]
            .iter()
            .rev()
            .find(|c| !c.is_ascii_whitespace())
            .is_some_and(|&c| c == b'=');
        if !follows_eq {
            continue;
        }
        // The end of the value; without one, the search goes on after the
        // opening quote.
        let mut q = p;
        while let Some(n) = memchr::memchr2(b[at], b'<', &b[q..]) {
            q += n + 1;
            if b[q - 1] == b[at] {
                p = q;
                break;
            }
            quoted_lt += 1;
            if quoted_lt > MAX_QUOTED_LT {
                return None;
            }
        }
    }
}

/// The tag that starts at the `<` at `b[i]`, or `None` when that `<` is
/// plain text: it opens no tag (see [`tag_open`]) or the tag is not closed
/// (see [`tag_close`]).
fn tag_at(b: &[u8], i: usize) -> Option<Tag> {
    let (name_at, closing) = tag_open(b, i)?;
    let close = tag_close(b, name_at)?;
    let name_len = b[name_at..close]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    Some(Tag {
        name_at,
        name_len,
        closing,
        end: close + 1,
    })
}

/// True when `b` holds a tag (see [`tag_at`]).
fn looks_like_html(b: &[u8]) -> bool {
    memchr::memchr_iter(b'<', b).any(|i| tag_at(b, i).is_some())
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
/// holds no tag keeps its line breaks. A `<` that does not start a tag (see
/// [`tag_at`]: no `>` before the next `<` outside a quoted attribute value,
/// or an address or URL in angle brackets) stays in the text with what
/// follows it. An unclosed comment or dropped element ends the text, and so
/// does a tag cut off by the end of an input that holds a tag.
///
/// Every search moves forward from the current position; a tag search
/// passes a bounded number of `<` (see [`tag_close`]), and the text after a
/// failed search for the end of a comment or dropped element is dropped, so
/// the run time is linear in the input.
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
        let Some(tag) = tag_at(b, i) else {
            // HTML that ends inside a tag: what is left is markup.
            let cut_off = is_html
                && tag_open(b, i).is_some()
                && memchr::memchr2(b'<', b'>', &b[i + 1..]).is_none();
            if cut_off {
                break;
            }
            raw.push('<');
            i += 1;
            continue;
        };
        let closing = tag.closing;
        let name = &lower[tag.name_at..tag.name_at + tag.name_len];
        i = tag.end;
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

/// A JSON number as an integer: an integer, or a number written with a
/// fraction of zero (`20.0`) that fits in an `i64`.
pub fn integer(v: &Value) -> Option<i64> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63
    v.as_i64().or_else(|| {
        let f = v.as_f64().filter(|_| !v.is_u64())?;
        (f.fract() == 0.0 && (-LIMIT..LIMIT).contains(&f)).then_some(f as i64)
    })
}

/// Unix seconds of a time argument: an integer (see [`integer`]) or a string
/// of digits (unix seconds), `YYYY-MM-DD` (midnight UTC) or
/// `YYYY-MM-DDThh:mm:ssZ`, in the years 1970 to 9999.
pub fn parse_time(v: &Value) -> Result<i64, String> {
    let t = match v {
        Value::Number(_) => integer(v),
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
    use std::time::Duration;

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
    fn less_than_signs_in_plain_text_are_kept() {
        assert_eq!(
            html_to_text("Budget: if x <y then stop.\nSecond line, the end."),
            "Budget: if x <y then stop.\nSecond line, the end."
        );
        // No `>` before the next `<`: text, also next to real tags.
        assert_eq!(
            html_to_text("<p>if a <b and c <d then <b>stop</b></p>"),
            "if a <b and c <d then stop"
        );
        assert_eq!(html_to_text("a <b <i>c</i>"), "a <b c");
        assert_eq!(html_to_text("x </y z"), "x </y z");
    }

    #[test]
    fn addresses_in_angle_brackets_are_kept() {
        assert_eq!(
            html_to_text("Contact John <john@example.com> or Ann <ann@example.com>"),
            "Contact John <john@example.com> or Ann <ann@example.com>"
        );
        assert_eq!(
            html_to_text("From: A <a.b-c@example.org>\nTo: B <b@example.org>"),
            "From: A <a.b-c@example.org>\nTo: B <b@example.org>"
        );
        assert_eq!(
            html_to_text("<p>Ann <ann@example.org></p><p>x</p>"),
            "Ann <ann@example.org>\nx"
        );
        // An `@` in an attribute does not make a tag an address.
        assert_eq!(
            html_to_text("<a href=\"mailto:ann@example.org\">Ann</a> <img src=cid:1@x>"),
            "Ann"
        );
    }

    #[test]
    fn less_than_and_greater_than_in_quoted_attribute_values() {
        assert_eq!(
            html_to_text("<img alt=\"1 < 2\" src=\"x.png\">tail"),
            "tail"
        );
        assert_eq!(
            html_to_text("<a href=\"x\" title=\"Reply to John <john@example.org>\">John</a> tail"),
            "John tail"
        );
        assert_eq!(html_to_text("<a title=\"a<b\">link</a> tail"), "link tail");
        assert_eq!(html_to_text("<a title='a<b>c'>link</a> tail"), "link tail");
        assert_eq!(
            html_to_text("<script data-x=\"<\">secret()</script>after"),
            "after"
        );
        assert_eq!(
            html_to_text("<style media=\"a<b\">p{color:red}</style>after"),
            "after"
        );
        assert_eq!(html_to_text("<p style=\"a>b\">text</p>"), "text");
        assert_eq!(html_to_text("<p title = \"a>b\" id=x>text</p>"), "text");
        // The other kind of quote inside a value is part of the value.
        assert_eq!(
            html_to_text("<a title=\"it's a<b\" id='\"<'>link</a>"),
            "link"
        );
    }

    #[test]
    fn quotes_that_open_no_attribute_value_are_plain_characters() {
        // Not after `=`.
        assert_eq!(
            html_to_text("<a href=x'y>link</a> it's <b>z</b>"),
            "link it's z"
        );
        assert_eq!(
            html_to_text("<p>a</p>if x <y, don't stop <b>now</b>, it's ok"),
            "a\nif x <y, don't stop now, it's ok"
        );
        // No closing quote.
        assert_eq!(html_to_text("<a title=\"x>link</a> tail"), "link tail");
        assert_eq!(html_to_text("<a title=\"x <b>y</b>"), "<a title=\"x y");
    }

    #[test]
    fn tag_with_too_many_less_than_signs_in_its_values_is_text() {
        let within = format!("<a title=\"{}\">x</a>", "<".repeat(MAX_QUOTED_LT));
        assert_eq!(html_to_text(&within), "x");
        let over = format!("<a title=\"{}\">x</a>", "<".repeat(MAX_QUOTED_LT + 1));
        assert_eq!(html_to_text(&over), over[..over.len() - 4]);
    }

    #[test]
    fn urls_in_angle_brackets_are_kept() {
        assert_eq!(
            html_to_text("Click here <https://example.org/a?b=c>\nSecond line\nThird"),
            "Click here <https://example.org/a?b=c>\nSecond line\nThird"
        );
        assert_eq!(
            html_to_text("<p>See <http://example.com/x> or <mailto:ann@example.org></p>"),
            "See <http://example.com/x> or <mailto:ann@example.org>"
        );
        // A name with a colon is still a tag.
        assert_eq!(html_to_text("a<o:p>b</o:p><v:shape id=\"://\">c"), "abc");
    }

    #[test]
    fn tag_cut_off_by_the_end_is_dropped_from_html_only() {
        assert_eq!(html_to_text("<p>before</p><a href=\"x"), "before");
        assert_eq!(
            html_to_text("<p>trunc</p><a href=\"https://example.org/very/long"),
            "trunc"
        );
        assert_eq!(html_to_text("<p>before</p></di"), "before");
        // Not the start of a tag: text.
        assert_eq!(html_to_text("<p>a</p>5 < 6"), "a\n5 < 6");
        assert_eq!(
            html_to_text("<p>a</p>Ann <ann@example.org"),
            "a\nAnn <ann@example.org"
        );
        // Input without a tag is plain text and keeps everything.
        assert_eq!(html_to_text("see <a href=\"x"), "see <a href=\"x");
        assert_eq!(html_to_text("if x <y then stop"), "if x <y then stop");
    }

    #[test]
    fn unclosed_script_or_comment_ends_the_text() {
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

    /// Inputs of `n` repeated units that would make a careless scanner
    /// search to the end of the input once per unit.
    fn hostile_inputs(n: usize) -> Vec<String> {
        [
            "<",
            "<a",
            "<a ",
            "</a",
            "<!",
            "<a@",
            "<a href=\"x\" ",
            "<a t=\"",
            "<a t=\"'\"'\"",
            "<a t=\"<",
            "<https://",
            "&#",
            "&",
            "<a>",
            "<!---->",
            "<style></style>",
        ]
        .iter()
        .map(|unit| unit.repeat(n))
        // One opening that is never closed, after a long run of each kind.
        .chain(
            ["<a>", "x ", "<"]
                .iter()
                .map(|unit| unit.repeat(n) + "<!--"),
        )
        // Quotes that are never closed, after or before a long run of
        // openings.
        .chain([
            "<a t=".repeat(n) + "\" '",
            "<a t=\"".to_string() + &"<a ".repeat(n),
        ])
        .chain(["<script>".repeat(n), "<!--".repeat(n), "<head>".repeat(n)])
        .collect()
    }

    /// CPU time used so far by the calling thread. Unlike the wall clock it
    /// does not grow while the thread waits for a CPU, so a busy machine
    /// does not stretch a long run more than a short one.
    fn thread_cpu_time() -> Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        assert_eq!(rc, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed");
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }

    /// Shortest thread CPU time of three runs of `html_to_text` over `input`.
    fn best_time(input: &str) -> Duration {
        (0..3)
            .map(|_| {
                let t = thread_cpu_time();
                std::hint::black_box(html_to_text(std::hint::black_box(input)));
                thread_cpu_time().saturating_sub(t)
            })
            .min()
            .unwrap()
    }

    #[test]
    fn pathological_input_results() {
        let lt = "<".repeat(1 << 20);
        assert_eq!(html_to_text(&lt), lt);
        // Never closed: text.
        let open = "<a".repeat(1 << 19);
        assert_eq!(html_to_text(&open), open);
        let scripts = "<script>".repeat(1 << 17);
        assert_eq!(html_to_text(&scripts), "");
        let comments = "<!--".repeat(1 << 18);
        assert_eq!(html_to_text(&comments), "");
        let amps = "&#".repeat(1 << 19);
        assert_eq!(html_to_text(&amps), amps);
    }

    #[test]
    fn pathological_input_is_linear() {
        // For every kind of input, eight times the length must not take
        // much more than eight times as long. A scan that is quadratic in
        // the input takes 64 times as long, whatever the machine's speed;
        // the limit of 24 times (plus a little for timer noise) lies
        // between the two. Times are CPU times of the test thread (see
        // `thread_cpu_time`), so the ratio holds on a loaded machine too.
        const N: usize = 1 << 14;
        let started = thread_cpu_time();
        for (small, large) in hostile_inputs(N).iter().zip(hostile_inputs(8 * N)) {
            let (t_small, t_large) = (best_time(small), best_time(&large));
            assert!(
                t_large <= t_small * 24 + Duration::from_millis(20),
                "not linear: {t_small:?} for n, {t_large:?} for 8 n, input starts {:?}",
                &small[..16]
            );
        }
        // And an absolute bound, far above what linear scans need.
        let used = thread_cpu_time().saturating_sub(started);
        assert!(
            used < Duration::from_secs(30),
            "too slow: {used:?} of CPU time"
        );
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
    fn parse_time_accepts_numbers_with_a_zero_fraction() {
        let v: Value = serde_json::from_str("1764579600.0").unwrap();
        assert_eq!(parse_time(&v), Ok(1_764_579_600));
        let v: Value = serde_json::from_str("1.7645796e9").unwrap();
        assert_eq!(parse_time(&v), Ok(1_764_579_600));
        for bad in ["1764579600.5", "-1.0", "1e30", "253402300800.0"] {
            let v: Value = serde_json::from_str(bad).unwrap();
            assert!(parse_time(&v).is_err(), "{bad}");
        }
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
