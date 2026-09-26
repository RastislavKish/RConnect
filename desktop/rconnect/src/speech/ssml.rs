//! Conversion of the SSML sent by speech-dispatcher into plain text and index marks.
//!
//! Orca wraps text in `<speak>` and inserts `<mark name="start:end"/>` before words;
//! speech-dispatcher adds its own `__spd_id_N` marks. Other elements are dropped and
//! only their text content is kept.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub text: String,
    /// `(offset in characters of text, mark name)`, in document order.
    pub marks: Vec<(u32, String)>,
}

pub fn parse(ssml: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut chars = 0u32;
    let mut rest = ssml;

    while let Some(c) = rest.chars().next() {
        match c {
            '<' => match rest.find('>') {
                Some(end) => {
                    if let Some(name) = mark_name(&rest[1..end]) {
                        parsed.marks.push((chars, name));
                    }
                    rest = &rest[end + 1..];
                }
                None => {
                    // Not markup after all; keep the rest as text.
                    chars += rest.chars().count() as u32;
                    parsed.text.push_str(rest);
                    rest = "";
                }
            },
            '&' => {
                let (decoded, len) = entity(rest).unwrap_or(('&', 1));
                parsed.text.push(decoded);
                chars += 1;
                rest = &rest[len..];
            }
            _ => {
                parsed.text.push(c);
                chars += 1;
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    parsed
}

/// Returns the name of a `<mark name="..."/>` tag, given the text between `<` and `>`.
fn mark_name(tag: &str) -> Option<String> {
    let attributes = tag.strip_prefix("mark")?;
    if !attributes.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let after_name = attributes.split_once("name")?.1.trim_start();
    let value = after_name.strip_prefix('=')?.trim_start();
    let quote = value.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let (raw, _) = value[1..].split_once(quote)?;
    Some(parse(raw).text)
}

/// Decodes the entity at the start of `text`, returning the character and the entity's
/// length in bytes.
fn entity(text: &str) -> Option<(char, usize)> {
    let end = text.find(';').filter(|&end| end <= 12)?;
    let body = &text[1..end];
    let c = match body {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let code = if let Some(hex) = body.strip_prefix("#x").or(body.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                body.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(code)?
        }
    };
    Some((c, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(list: &[(u32, &str)]) -> Vec<(u32, String)> {
        list.iter()
            .map(|&(offset, name)| (offset, name.to_string()))
            .collect()
    }

    #[test]
    fn orca_style_markup() {
        let parsed = parse(
            "<speak><mark name=\"0:5\"/>Hello <mark name=\"6:11\"/>world<mark name=\"__spd_id_0\"/></speak>",
        );
        assert_eq!(parsed.text, "Hello world");
        assert_eq!(
            parsed.marks,
            marks(&[(0, "0:5"), (6, "6:11"), (11, "__spd_id_0")])
        );
    }

    #[test]
    fn entities() {
        let parsed = parse("<speak>a &lt;b&gt; &amp; &quot;c&apos; &#233;&#x20AC;</speak>");
        assert_eq!(parsed.text, "a <b> & \"c' é€");
    }

    #[test]
    fn offsets_count_characters_not_bytes() {
        let parsed = parse("<speak>čšž<mark name='x'/>a</speak>");
        assert_eq!(parsed.text, "čšža");
        assert_eq!(parsed.marks, marks(&[(3, "x")]));
    }

    #[test]
    fn other_elements_are_dropped() {
        let parsed = parse(
            "<?xml version=\"1.0\"?><speak><s>One</s> <marker/>two<break time=\"1s\"/></speak>",
        );
        assert_eq!(parsed.text, "One two");
        assert!(parsed.marks.is_empty());
    }

    #[test]
    fn stray_characters_are_kept() {
        assert_eq!(parse("a & b").text, "a & b");
        assert_eq!(parse("1 < 2").text, "1 < 2");
        assert_eq!(parse("&unknown; &#xZZ;").text, "&unknown; &#xZZ;");
        assert_eq!(parse("&ččččččč;").text, "&ččččččč;");
    }
}
