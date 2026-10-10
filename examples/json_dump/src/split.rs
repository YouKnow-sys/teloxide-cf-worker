//! Splitting the JSON dump into Telegram-sized messages.

const BUDGET: usize = 3900;

/// Length of `c` after teloxide's HTML escaping (`&amp;` `&lt;` `&gt;`).
fn escaped_len(c: char) -> usize {
    match c {
        '&' => 5,
        '<' | '>' => 4,
        _ => 1,
    }
}

/// Chunks of `json` that each fit one Telegram message (4096 chars) after
/// HTML escaping and the code-block tags, cutting at line breaks when
/// possible so the pieces stay readable JSON.
pub fn chunks(json: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let (mut chunk, mut len) = (String::new(), 0);
    for c in json.chars() {
        let c_len = escaped_len(c);
        if len + c_len > BUDGET {
            // Cut after the last line break, or right here if there is none.
            let at = chunk.rfind('\n').map_or(chunk.len(), |i| i + 1);
            let rest = chunk.split_off(at);
            chunks.push(std::mem::replace(&mut chunk, rest));
            len = chunk.chars().map(escaped_len).sum();
        }
        chunk.push(c);
        len += c_len;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}
