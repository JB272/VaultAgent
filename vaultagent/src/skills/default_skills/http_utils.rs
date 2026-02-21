use reqwest::Client;
use serde_json::json;

/// Fetches a URL and returns its stripped-text content as a JSON string.
/// Content is truncated to `max_chars`.
pub async fn fetch_page(client: &Client, url: &str, max_chars: usize) -> String {
    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return json!({ "ok": false, "error": format!("HTTP error: {}", e) }).to_string();
        }
    };

    let status = response.status().as_u16();
    if !response.status().is_success() {
        return json!({ "ok": false, "error": format!("HTTP {}", status) }).to_string();
    }

    let body = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return json!({ "ok": false, "error": format!("Failed to read response body: {}", e) })
                .to_string();
        }
    };

    let text = strip_html(&body);
    let truncated = if text.len() > max_chars {
        format!(
            "{}...\n[truncated, {} total characters]",
            &text[..max_chars],
            text.len()
        )
    } else {
        text
    };

    json!({
        "ok": true,
        "url": url,
        "content": truncated,
    })
    .to_string()
}

/// Strips HTML tags, script/style blocks and normalises whitespace.
pub fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut last_was_whitespace = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if starts_with_at(&lower_chars, i, "<script") {
            in_script = true;
            in_tag = true;
        } else if starts_with_at(&lower_chars, i, "</script") {
            in_script = false;
            in_tag = true;
        } else if starts_with_at(&lower_chars, i, "<style") {
            in_style = true;
            in_tag = true;
        } else if starts_with_at(&lower_chars, i, "</style") {
            in_style = false;
            in_tag = true;
        } else if chars[i] == '<' {
            in_tag = true;
        }

        if chars[i] == '>' && in_tag {
            in_tag = false;
            i += 1;
            continue;
        }

        if !in_tag && !in_script && !in_style {
            let ch = chars[i];
            if ch.is_whitespace() {
                if !last_was_whitespace {
                    result.push(' ');
                    last_was_whitespace = true;
                }
            } else {
                result.push(ch);
                last_was_whitespace = false;
            }
        }

        i += 1;
    }

    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn starts_with_at(chars: &[char], pos: usize, needle: &str) -> bool {
    let needle_chars: Vec<char> = needle.chars().collect();
    if pos + needle_chars.len() > chars.len() {
        return false;
    }
    for (j, nc) in needle_chars.iter().enumerate() {
        if chars[pos + j] != *nc {
            return false;
        }
    }
    true
}
