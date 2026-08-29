//! HTML rendering: the page shell, escaping, and the small cell
//! formatters shared by the list views.

use super::AppState;

const CSS: &str = r#"
body { font-family: system-ui, sans-serif; margin: 0; color: #222; background: #fafafa; }
header { background: #2a3f5f; color: #fff; padding: 0.75rem 1.25rem; }
header a { color: #fff; text-decoration: none; margin-right: 1rem; font-weight: 500; }
header a:hover { text-decoration: underline; }
main { max-width: 960px; margin: 1.5rem auto; padding: 0 1.25rem; }
h1 { margin-top: 0; }
table { border-collapse: collapse; width: 100%; background: #fff; }
th, td { padding: 0.5rem 0.75rem; text-align: left; border-bottom: 1px solid #eee; vertical-align: top; }
th { background: #f2f4f8; font-weight: 600; }
tr:hover td { background: #fbfcff; }
pre { background: #f5f5f7; padding: 1rem; overflow: auto; }
.badge { display: inline-block; padding: 0.1rem 0.5rem; border-radius: 999px; font-size: 0.8rem; background: #e8eef7; color: #2a3f5f; }
.muted { color: #777; }
.empty { padding: 2rem; text-align: center; color: #777; background: #fff; border: 1px dashed #ddd; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 1rem; }
.card { background: #fff; padding: 1rem; border-radius: 8px; box-shadow: 0 1px 2px rgba(0,0,0,0.05); }
.card h2 { margin: 0 0 0.25rem; font-size: 1.1rem; }
.card .n { font-size: 2rem; font-weight: 600; color: #2a3f5f; }
a { color: #2a3f5f; }
footer { max-width: 960px; margin: 3rem auto 1.5rem; padding: 1rem 1.25rem; border-top: 1px solid #e0e0e0; color: #777; font-size: 0.85rem; }
footer a { color: #777; }
"#;

pub(super) fn page(state: &AppState, title: &str, body: &str) -> String {
    let home = state.url("/");
    let mut nav = format!("<a href=\"{}\">mailsift</a>\n", esc(&home));
    for (label, href, present) in [
        ("events", "/events", state.events_dir().is_some()),
        ("bills", "/bills", state.bills_dir().is_some()),
        ("parcels", "/parcels", state.parcels_dir().is_some()),
        ("receipts", "/receipts", state.receipts_dir().is_some()),
        (
            "subscriptions",
            "/subscriptions",
            state.subscriptions_dir().is_some(),
        ),
        ("tickets", "/tickets", state.tickets_dir().is_some()),
    ] {
        if present {
            nav.push_str(&format!(
                "<a href=\"{}\">{}</a>\n",
                esc(&state.url(href)),
                esc(label),
            ));
        }
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>{title} - mailsift</title>\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <style>{CSS}</style>\n</head>\n<body>\n\
         <header>\n{nav}</header>\n\
         <main>\n<h1>{title}</h1>\n{body}\n</main>\n\
         <footer>\n\
         <a href=\"https://github.com/jelmer/mailsift\">mailsift</a> \
         &copy; 2025-2026 Jelmer Vernoo&#307;j \
         &lt;<a href=\"mailto:jelmer@jelmer.uk\">jelmer@jelmer.uk</a>&gt;\n\
         </footer>\n\
         </body>\n</html>",
        title = esc(title),
    )
}

pub(super) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render a "links" cell for a list row: always a link to the raw
/// JSON, plus an optional "open" link to the artifact's vendor URL.
/// `open` links carry `rel=\"noopener noreferrer\"` since they leave
/// our origin.
pub(super) fn links_cell(json_href: &str, vendor: Option<&str>) -> String {
    let mut out = format!("<a href=\"{}\">json</a>", esc(json_href));
    if let Some(url) = vendor {
        out.push_str(&format!(
            " &middot; <a href=\"{}\" rel=\"noopener noreferrer\">open</a>",
            esc(url),
        ));
    }
    out
}

pub(super) fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(2 * 1024 * 1024), "2.0 MB");
    }
}
