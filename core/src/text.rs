//! Turns the HTML of a job description into plain text a model can read.

/// Elements that start a new line in the text.
const BLOCKS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "table",
    "section",
    "blockquote",
];

/// The text of `html`, one line per block, with entities decoded and runs of space collapsed.
/// Script and style content is dropped. List items start with `- `.
pub fn plain(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&decoded(&rest[..open]));
        rest = &rest[open..];
        let Some(close) = rest.find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[1..close];
        rest = &rest[close + 1..];
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if (name == "script" || name == "style") && !tag.starts_with('/') {
            let end = format!("</{name}");
            rest = match rest.to_ascii_lowercase().find(&end) {
                Some(at) => &rest[at..],
                None => "",
            };
            continue;
        }
        if BLOCKS.contains(&name.as_str()) {
            out.push('\n');
            if name == "li" && !tag.starts_with('/') {
                out.push_str("- ");
            }
        }
    }
    out.push_str(&decoded(rest));
    tidy(&out)
}

/// Collapses spaces inside each line and drops empty lines.
fn tidy(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty() && line != "-")
        .collect();
    lines.join("\n")
}

/// `text` with HTML character references replaced by the characters they name.
/// A reference this function does not know stays as it is.
pub fn decoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let named = rest[1..]
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| Some((character(&rest[1..=end])?, end + 2)));
        match named {
            Some((character, length)) => {
                out.push(character);
                rest = &rest[length..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn character(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        // A no-break space reads as a space.
        return if code == 0xa0 { Some(' ') } else { char::from_u32(code) };
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "rsquo" | "lsquo" => '\'',
        "rdquo" | "ldquo" => '"',
        "hellip" => '…',
        "bull" => '•',
        "middot" => '·',
        "trade" => '™',
        "reg" => '®',
        "copy" => '©',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_become_lines_and_list_items_get_a_dash() {
        let html = "<h2>About</h2><p>We build <b>tools</b>&nbsp;for   teams.</p>\
                    <ul><li>Rust &amp; Go</li><li>5&#43; years</li></ul><br/>Remote&#x2014;US";
        assert_eq!(
            plain(html),
            "About\nWe build tools for teams.\n- Rust & Go\n- 5+ years\nRemote—US"
        );
    }

    #[test]
    fn scripts_and_styles_are_dropped() {
        let html = "<style>p { color: red }</style><p>Kept</p><script>var a = '<p>no</p>';</script>after";
        assert_eq!(plain(html), "Kept\nafter");
    }

    #[test]
    fn an_unknown_or_broken_reference_stays_as_written() {
        assert_eq!(decoded("R&D &bogus; &#xZZ; & &lt;p&gt;"), "R&D &bogus; &#xZZ; & <p>");
    }
}
