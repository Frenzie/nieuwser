use crate::models::Item;

/// Plain-text rendering for the first reader view. The structured document IR comes later.
pub fn html_to_text(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let text = html2text::from_read(html.as_bytes(), 100);
    let mut out = String::new();
    let mut blank_lines = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_lines += 1;
            if blank_lines <= 1 {
                out.push('\n');
            }
        } else {
            blank_lines = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

pub fn item_text(item: &Item) -> String {
    html_to_text(item.body_html())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup_and_keeps_paragraphs() {
        let text = html_to_text("<p>Hello <b>world</b>.</p><p>Second paragraph.</p>");
        assert!(text.contains("Hello world."));
        assert!(text.contains("Second paragraph."));
        assert!(!text.contains('<'));
    }
}
