//! Mumble chat is HTML (a Qt subset). This turns it into plain text: tags
//! dropped, line breaks kept, entities decoded, images named.

pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            let Some(end) = rest.find('>') else {
                // Not a tag after all.
                out.push_str(rest);
                break;
            };
            let tag = rest[1..end].trim().to_ascii_lowercase();
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("");
            match name {
                "br" => out.push('\n'),
                "p" | "div" | "li" | "tr" if !tag.starts_with('/') && !out.is_empty() => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                "img" => out.push_str("[image]"),
                _ => {}
            }
            rest = &rest[end + 1..];
        } else if c == '&' {
            match decode_entity(rest) {
                Some((decoded, len)) => {
                    out.push(decoded);
                    rest = &rest[len..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.trim().to_owned()
}

/// Plain text as Mumble chat HTML, so `<` shows as itself.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br/>"),
            c => out.push(c),
        }
    }
    out
}

/// The entity at the start of `text`, and its length.
fn decode_entity(text: &str) -> Option<(char, usize)> {
    // Entities are short; a ';' further on isn't this one's.
    let end = text.char_indices().take(12).find(|&(_, c)| c == ';')?.0;
    let name = &text[1..end];
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let code = if let Some(hex) = name.strip_prefix("#x").or(name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                name.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(code)?
        }
    };
    Some((c, end + 1))
}

#[cfg(test)]
mod tests {
    use super::{escape, to_text};

    #[test]
    fn escapes_and_round_trips() {
        let plain = "a < b & \"c\"\nnext";
        assert_eq!(to_text(&escape(plain)), plain);
    }

    #[test]
    fn strips_tags_and_keeps_breaks() {
        assert_eq!(
            to_text("hello <b>there</b><br/>friend"),
            "hello there\nfriend"
        );
        assert_eq!(to_text("<p>one</p><p>two</p>"), "one\ntwo");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(to_text("a &amp; b &lt;3 &#233;&#x41;"), "a & b <3 éA");
        assert_eq!(to_text("fish & chips"), "fish & chips");
        assert_eq!(to_text("&bogus; ok"), "&bogus; ok");
        assert_eq!(to_text("ends &amp;"), "ends &");
    }

    #[test]
    fn names_images() {
        assert_eq!(
            to_text("look <img src=\"data:image/png;base64,AAAA\"/>"),
            "look [image]"
        );
    }

    #[test]
    fn survives_stray_angle_brackets() {
        assert_eq!(to_text("1 < 2"), "1 < 2");
    }
}
