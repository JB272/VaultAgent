/// Converts LLM Markdown output to Telegram HTML.
///
/// Telegram HTML supports: <b>, <i>, <u>, <s>, <code>, <pre>, <a href="...">.
/// Everything else must be HTML-escaped.
///
/// Handles:
///   - ``` code fences (with optional language tag)  → <pre><code>
///   - `inline code`                                  → <code>
///   - **bold** / __bold__                            → <b>
///   - *italic* (not list markers)                    → <i>
///   - ~~strikethrough~~                              → <s>
///   - [text](url)                                    → <a href="url">
///   - # / ## / ### headings                          → <b>
///   - - item / * item list bullets                   → • item
///   - > blockquote                                   → │ text
///   - --- horizontal rules                           → ─────────────
pub fn md_to_telegram_html(input: &str) -> String {
    // Split on triple-backtick code fences.
    // Even indices → prose, odd indices → code blocks.
    let parts: Vec<&str> = input.split("```").collect();
    let mut out = String::with_capacity(input.len() * 2);

    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 0 {
            out.push_str(&convert_prose(part));
        } else {
            // First line may be a language tag (e.g. "rust\n...")
            let (lang, code) = split_lang(part);
            let escaped = html_escape(code.trim_matches('\n'));
            if escaped.is_empty() {
                continue;
            }
            if lang.is_empty() {
                out.push_str("<pre><code>");
                out.push_str(&escaped);
                out.push_str("</code></pre>");
            } else {
                out.push_str("<pre><code>");
                out.push_str(&escaped);
                out.push_str("</code></pre>");
            }
        }
    }

    out.trim().to_string()
}

fn split_lang(block: &str) -> (&str, &str) {
    match block.find('\n') {
        Some(nl) => (&block[..nl], &block[nl + 1..]),
        None => ("", block),
    }
}

/// Process a prose block (no ``` fences): split on inline code backticks first.
fn convert_prose(text: &str) -> String {
    let parts: Vec<&str> = text.split('`').collect();
    let mut out = String::with_capacity(text.len() * 2);

    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 0 {
            out.push_str(&convert_block(part));
        } else {
            out.push_str("<code>");
            out.push_str(&html_escape(part));
            out.push_str("</code>");
        }
    }
    out
}

/// Process a plain-text prose section line by line.
fn convert_block(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut lines = text.split('\n').peekable();

    while let Some(line) = lines.next() {
        out.push_str(&convert_line(line));
        if lines.peek().is_some() {
            out.push('\n');
        }
    }
    out
}

/// Apply block-level markdown to a single line.
fn convert_line(line: &str) -> String {
    // Headings
    if let Some(rest) = line.strip_prefix("### ") {
        return format!("<b>{}</b>", convert_inline(rest));
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return format!("<b>{}</b>", convert_inline(rest));
    }
    if let Some(rest) = line.strip_prefix("# ") {
        return format!("<b>{}</b>", convert_inline(rest));
    }

    // Horizontal rules
    if matches!(line, "---" | "***" | "___") {
        return "─────────────".to_string();
    }

    // Unordered list items (- item  or  * item at line start)
    if let Some(rest) = line.strip_prefix("- ") {
        return format!("• {}", convert_inline(rest));
    }
    if let Some(rest) = line.strip_prefix("* ") {
        return format!("• {}", convert_inline(rest));
    }

    // Blockquote
    if let Some(rest) = line.strip_prefix("> ") {
        return format!("│ {}", convert_inline(rest));
    }

    convert_inline(line)
}

/// Apply inline markdown: HTML-escape first, then bold/italic/links/strikethrough.
fn convert_inline(text: &str) -> String {
    // HTML-escape first so user content can't inject tags.
    // Markdown markers (*, _, ~, [, ], (, )) are not HTML-special, so they survive.
    let s = html_escape(text);

    // Bold must come before italic so ** is processed before *.
    let s = replace_pair(&s, "**", "<b>", "</b>");
    let s = replace_pair(&s, "__", "<b>", "</b>");
    // Italic
    let s = replace_pair(&s, "*", "<i>", "</i>");
    // Strikethrough
    let s = replace_pair(&s, "~~", "<s>", "</s>");
    // Markdown links [text](url)
    let s = convert_links(&s);

    s
}

/// Replaces paired `marker` occurrences with `open`/`close` HTML tags.
/// Unmatched markers are left as-is.
fn replace_pair(text: &str, marker: &str, open: &str, close: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut remaining = text;

    while let Some(start) = remaining.find(marker) {
        result.push_str(&remaining[..start]);
        let after_open = &remaining[start + marker.len()..];

        if let Some(end) = after_open.find(marker) {
            // Only treat as formatting if the inner content is non-empty
            // and doesn't start/end with a space (avoids * bullet * style false-positives).
            let inner = &after_open[..end];
            if !inner.is_empty() && !inner.starts_with(' ') && !inner.ends_with(' ') {
                result.push_str(open);
                result.push_str(inner);
                result.push_str(close);
                remaining = &after_open[end + marker.len()..];
            } else {
                result.push_str(marker);
                remaining = after_open;
            }
        } else {
            // No closing marker — leave as-is.
            result.push_str(marker);
            remaining = after_open;
        }
    }
    result.push_str(remaining);
    result
}

/// Converts [text](url) Markdown links to <a href="url">text</a>.
fn convert_links(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut remaining = text;

    while let Some(bracket_start) = remaining.find('[') {
        let before = &remaining[..bracket_start];
        let after_bracket = &remaining[bracket_start + 1..];

        if let Some(bracket_end) = after_bracket.find(']') {
            let link_text = &after_bracket[..bracket_end];
            let after_close = &after_bracket[bracket_end + 1..];

            if let Some(rest) = after_close.strip_prefix('(') {
                if let Some(paren_end) = rest.find(')') {
                    let url = &rest[..paren_end];
                    result.push_str(before);
                    result.push_str(&format!("<a href=\"{}\">{}</a>", url, link_text));
                    remaining = &rest[paren_end + 1..];
                    continue;
                }
            }
        }

        // Not a valid link — output `[` literally and continue.
        result.push_str(before);
        result.push('[');
        remaining = after_bracket;
    }
    result.push_str(remaining);
    result
}

/// HTML-escapes the 4 required characters.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
