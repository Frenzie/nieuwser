use scraper::{ElementRef, Html, Node};
use url::Url;

use crate::models::Item;

/// A block-level piece of an article. Inline text uses CommonMark markup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    ListItem { depth: usize },
    Quote,
    Code,
    Rule,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub label: String,
    pub text: String,
}

/// Converts article HTML into blocks for native rendering.
pub fn html_to_blocks(html: &str, base_url: Option<&str>) -> Vec<Block> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let base = base_url.and_then(|url| Url::parse(url).ok());
    let document = Html::parse_fragment(html);
    let mut blocks = Vec::new();
    collect_container(&document.root_element(), base.as_ref(), &mut blocks);
    blocks
}

pub fn html_to_text(html: &str) -> String {
    html_to_blocks(html, None)
        .into_iter()
        .map(|block| block.text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn item_text(item: &Item) -> String {
    html_to_text(item.body_html())
}

fn collect_container(el: &ElementRef, base: Option<&Url>, blocks: &mut Vec<Block>) {
    let mut inline = String::new();
    for child in el.children() {
        match child.value() {
            Node::Text(text) => inline.push_str(&escape_text(&text.text)),
            Node::Element(_) => {
                let Some(child) = ElementRef::wrap(child) else {
                    continue;
                };
                let name = child.value().name();
                if is_block_tag(name) {
                    flush_inline(&mut inline, blocks);
                    collect_element(&child, base, blocks);
                } else {
                    inline.push_str(&inline_element(&child, base));
                }
            }
            _ => {}
        }
    }
    flush_inline(&mut inline, blocks);
}

fn collect_element(el: &ElementRef, base: Option<&Url>, blocks: &mut Vec<Block>) {
    let name = el.value().name();
    if is_skipped(name) {
        return;
    }
    if let Some(level) = heading_level(name) {
        let text = inline_children(el, base);
        if !text.trim().is_empty() {
            blocks.push(Block {
                kind: BlockKind::Heading(level),
                label: String::new(),
                text: text.trim().to_string(),
            });
        }
        return;
    }

    match name {
        "p" | "figcaption" | "dt" | "dd" => {
            let text = inline_children(el, base);
            if !text.trim().is_empty() {
                blocks.push(Block {
                    kind: BlockKind::Paragraph,
                    label: String::new(),
                    text: text.trim().to_string(),
                });
            }
        }
        "ul" => collect_list(el, false, 0, base, blocks),
        "ol" => collect_list(el, true, 0, base, blocks),
        "blockquote" => {
            let text = inline_children(el, base);
            if !text.trim().is_empty() {
                blocks.push(Block {
                    kind: BlockKind::Quote,
                    label: String::new(),
                    text: text.trim().to_string(),
                });
            }
        }
        "pre" => {
            let text = el.text().collect::<String>();
            let text = text.trim_end_matches('\n');
            if !text.trim().is_empty() {
                blocks.push(Block {
                    kind: BlockKind::Code,
                    label: String::new(),
                    text: text.to_string(),
                });
            }
        }
        "hr" => blocks.push(Block {
            kind: BlockKind::Rule,
            label: String::new(),
            text: String::new(),
        }),
        "img" => {
            if let Some(block) = image_block(el, base) {
                blocks.push(block);
            }
        }
        _ => collect_container(el, base, blocks),
    }
}

fn collect_list(
    el: &ElementRef,
    ordered: bool,
    depth: usize,
    base: Option<&Url>,
    blocks: &mut Vec<Block>,
) {
    let mut number = 0;
    for child in el.children() {
        let Some(item) = ElementRef::wrap(child) else {
            continue;
        };
        if item.value().name() != "li" {
            continue;
        }
        number += 1;
        let text = inline_children_filtered(&item, base, true);
        if !text.trim().is_empty() {
            blocks.push(Block {
                kind: BlockKind::ListItem { depth },
                label: if ordered {
                    format!("{number}.")
                } else {
                    "•".into()
                },
                text: text.trim().to_string(),
            });
        }
        for nested in item.children() {
            let Some(nested) = ElementRef::wrap(nested) else {
                continue;
            };
            match nested.value().name() {
                "ul" => collect_list(&nested, false, depth + 1, base, blocks),
                "ol" => collect_list(&nested, true, depth + 1, base, blocks),
                _ => {}
            }
        }
    }
}

fn image_block(el: &ElementRef, base: Option<&Url>) -> Option<Block> {
    let raw = el
        .value()
        .attr("src")
        .filter(|src| !src.trim().is_empty())
        .or_else(|| el.value().attr("data-src"))?;
    let src = resolve_url(raw, base, false)?;
    let alt = el.value().attr("alt").unwrap_or("").trim();
    Some(Block {
        kind: BlockKind::Image,
        label: if alt.is_empty() {
            "Image".into()
        } else {
            alt.to_string()
        },
        text: format!("[Open image](<{}>)", escape_url(&src)),
    })
}

fn inline_children(el: &ElementRef, base: Option<&Url>) -> String {
    inline_children_filtered(el, base, false)
}

fn inline_children_filtered(el: &ElementRef, base: Option<&Url>, skip_lists: bool) -> String {
    let mut out = String::new();
    for child in el.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&escape_text(&text.text)),
            Node::Element(_) => {
                let Some(child) = ElementRef::wrap(child) else {
                    continue;
                };
                if skip_lists && matches!(child.value().name(), "ul" | "ol") {
                    continue;
                }
                out.push_str(&inline_element(&child, base));
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

fn inline_element(el: &ElementRef, base: Option<&Url>) -> String {
    let name = el.value().name();
    if is_skipped(name) || matches!(name, "ul" | "ol") {
        return String::new();
    }
    let inner = inline_children(el, base);
    match name {
        "strong" | "b" => emphasis("**", &inner),
        "em" | "i" => emphasis("*", &inner),
        "s" | "del" | "strike" => emphasis("~~", &inner),
        "u" => {
            let text = inner.trim();
            if text.is_empty() {
                String::new()
            } else {
                format!("<u>{text}</u>")
            }
        }
        "code" => inline_code(el),
        "a" => {
            let label = if inner.trim().is_empty() {
                el.value().attr("href").map(escape_text).unwrap_or_default()
            } else {
                inner.trim().to_string()
            };
            match el
                .value()
                .attr("href")
                .and_then(|href| resolve_url(href, base, true))
            {
                Some(url) if !label.is_empty() => {
                    format!("[{label}](<{}>)", escape_url(&url))
                }
                _ => label,
            }
        }
        "br" => "  \n".into(),
        "img" => el.value().attr("alt").map(escape_text).unwrap_or_default(),
        "p" | "div" | "section" | "article" => {
            if inner.trim().is_empty() {
                String::new()
            } else {
                format!("\n\n{}", inner.trim())
            }
        }
        _ => inner,
    }
}

fn inline_code(el: &ElementRef) -> String {
    let text = el.text().collect::<String>();
    let text = collapse_spaces(&text);
    if text.is_empty() {
        return String::new();
    }
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest + 1);
    if text.starts_with('`') || text.ends_with('`') {
        format!("{ticks} {text} {ticks}")
    } else {
        format!("{ticks}{text}{ticks}")
    }
}

fn emphasis(marker: &str, inner: &str) -> String {
    let text = inner.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if inner.starts_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(marker);
    out.push_str(text);
    out.push_str(marker);
    if inner.ends_with(char::is_whitespace) {
        out.push(' ');
    }
    out
}

fn flush_inline(buffer: &mut String, blocks: &mut Vec<Block>) {
    let text = buffer.trim().to_string();
    buffer.clear();
    if !text.is_empty() {
        blocks.push(Block {
            kind: BlockKind::Paragraph,
            label: String::new(),
            text,
        });
    }
}

fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            if matches!(
                c,
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '~' | '|' | '!'
            ) {
                out.push('\\');
            }
            out.push(c);
            space = false;
        }
    }
    out
}

fn collapse_spaces(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out.trim().to_string()
}

fn escape_url(url: &str) -> String {
    url.replace(' ', "%20")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

fn resolve_url(raw: &str, base: Option<&Url>, allow_mailto: bool) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(_) => base?.join(raw).ok()?,
    };
    match url.scheme() {
        "http" | "https" => Some(url.to_string()),
        "mailto" if allow_mailto => Some(url.to_string()),
        _ => None,
    }
}

fn heading_level(name: &str) -> Option<u8> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "html"
            | "img"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "noscript"
            | "iframe"
            | "svg"
            | "form"
            | "button"
            | "input"
            | "select"
            | "textarea"
            | "video"
            | "audio"
            | "canvas"
            | "template"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_headings_and_inline_markup() {
        let blocks = html_to_blocks(
            "<h1>Title</h1><p>Hello <b>world</b> and <a href='/x'>link</a>.</p><script>bad()</script>",
            Some("https://example.com/post"),
        );
        assert_eq!(blocks[0].kind, BlockKind::Heading(1));
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(
            blocks[1].text,
            "Hello **world** and [link](<https://example.com/x>)."
        );
        assert!(!format!("{blocks:?}").contains("bad()"));
    }

    #[test]
    fn builds_lists_quotes_code_and_images() {
        let blocks = html_to_blocks(
            "<ul><li>One</li><li>Two<ul><li>Nested</li></ul></li></ul>\
             <blockquote><p>Q1</p><p>Q2</p></blockquote>\
             <pre>let x = 1;</pre><hr><img src='pic.png' alt='Pic'>",
            Some("https://example.com/a/b"),
        );
        assert_eq!(blocks[0].kind, BlockKind::ListItem { depth: 0 });
        assert_eq!(blocks[0].label, "•");
        assert_eq!(blocks[0].text, "One");
        assert_eq!(blocks[2].kind, BlockKind::ListItem { depth: 1 });
        assert_eq!(blocks[2].text, "Nested");
        assert_eq!(blocks[3].kind, BlockKind::Quote);
        assert_eq!(blocks[3].text, "Q1\n\nQ2");
        assert_eq!(blocks[4].kind, BlockKind::Code);
        assert_eq!(blocks[4].text, "let x = 1;");
        assert_eq!(blocks[5].kind, BlockKind::Rule);
        assert_eq!(blocks[6].kind, BlockKind::Image);
        assert_eq!(blocks[6].label, "Pic");
        assert!(blocks[6].text.contains("https://example.com/a/pic.png"));
    }

    #[test]
    fn orders_lists_and_skips_unsafe_links() {
        let blocks = html_to_blocks(
            "<ol><li>First</li><li><a href='javascript:alert(1)'>Second</a></li></ol>\
             <p><a href='mailto:me@example.com'>Mail</a></p>",
            None,
        );
        assert_eq!(blocks[0].label, "1.");
        assert_eq!(blocks[1].text, "Second");
        assert_eq!(blocks[2].text, "[Mail](<mailto:me@example.com>)");
    }

    #[test]
    fn keeps_plain_text_for_legacy_helpers() {
        let text = html_to_text("<p>Hello <b>world</b>.</p><p>Second paragraph.</p>");
        assert!(text.contains("Hello **world**."));
        assert!(text.contains("Second paragraph."));
        assert!(!text.contains('<'));
    }
}
