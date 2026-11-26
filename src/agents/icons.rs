//! Agent icons (`AgentInfo::icon`): SVG markup, square, sized by its
//! container (a `viewBox`, no width or height).
//!
//! The built-in agents' icons are their makers' marks, from lobe-icons (MIT)
//! or the maker's own site; see `assets/agent-icons/README.md`. Single-colour
//! ones draw in `currentColor`, so inlined they follow the text colour. Ids
//! inside each are prefixed `agl-<agent>-`, so several can share a page.
//!
//! A custom agent may name its own (`custom_agents[].icon`); agl passes it on
//! only if it looks like plain SVG (see [`safe`]).

/// The built-in icon for an agent id.
pub fn builtin(id: &str) -> Option<&'static str> {
    Some(match id {
        "claude" => include_str!("../../assets/agent-icons/claude.svg"),
        "codex" => include_str!("../../assets/agent-icons/codex.svg"),
        "gemini" => include_str!("../../assets/agent-icons/gemini.svg"),
        "antigravity" => include_str!("../../assets/agent-icons/antigravity.svg"),
        "opencode" => include_str!("../../assets/agent-icons/opencode.svg"),
        "kimi" => include_str!("../../assets/agent-icons/kimi.svg"),
        "grok" => include_str!("../../assets/agent-icons/grok.svg"),
        "droid" => include_str!("../../assets/agent-icons/droid.svg"),
        "openclaw" => include_str!("../../assets/agent-icons/openclaw.svg"),
        "hermes" => include_str!("../../assets/agent-icons/hermes.svg"),
        "omp" => include_str!("../../assets/agent-icons/omp.svg"),
        "cursor" => include_str!("../../assets/agent-icons/cursor.svg"),
        "pi" => include_str!("../../assets/agent-icons/pi.svg"),
        _ => return None,
    })
}

/// Largest custom icon passed on.
const MAX_CUSTOM: usize = 32 * 1024;

/// An agent's icon: a custom agent's own when it is [`safe`], else the
/// built-in one for its id (a custom agent replacing a built-in keeps that
/// icon unless it names another).
pub fn for_agent(id: &str, custom: Option<&str>) -> Option<String> {
    match custom.map(str::trim).filter(|s| !s.is_empty()) {
        Some(svg) if safe(svg) => Some(svg.to_string()),
        Some(_) => {
            eprintln!("custom agent `{id}`: icon ignored (it must be plain SVG markup under {MAX_CUSTOM} bytes, with no scripts, event handlers, external references or foreignObject)");
            builtin(id).map(String::from)
        }
        None => builtin(id).map(String::from),
    }
}

/// Plain SVG a page can inline: one `<svg>` element, no scripts, event
/// handlers, `javascript:` links, external references or embedded HTML.
/// Apps should still treat a machine's icon as untrusted (see the docs); this
/// only keeps agl from passing on what is plainly not an icon.
fn safe(svg: &str) -> bool {
    let lower = svg.to_ascii_lowercase();
    let starts = lower.starts_with("<svg") || (lower.starts_with("<?xml") && lower.contains("<svg"));
    let forbidden = ["<script", "javascript:", "<foreignobject", "<iframe", "<object", "<embed", "<use", "href=\"http", "href='http", "url(http", "@import"];
    starts
        && lower.trim_end().ends_with("</svg>")
        && svg.len() <= MAX_CUSTOM
        && !forbidden.iter().any(|f| lower.contains(f))
        && !has_event_handler(&lower)
}

/// An `on…=` attribute anywhere (onload, onclick, …).
fn has_event_handler(lower: &str) -> bool {
    let b = lower.as_bytes();
    (0..b.len().saturating_sub(3)).any(|i| {
        b[i].is_ascii_whitespace() && b[i + 1] == b'o' && b[i + 2] == b'n' && {
            let rest = &b[i + 3..];
            let name = rest.iter().take_while(|c| c.is_ascii_alphabetic()).count();
            name > 0 && rest[name..].iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'=')
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_agent_has_a_plain_svg_icon() {
        for d in super::super::registry(&crate::config::Config::default()) {
            let svg = builtin(&d.id).unwrap_or_else(|| panic!("{} has no icon", d.id));
            assert!(safe(svg), "{} icon is not plain SVG", d.id);
            assert!(svg.contains("viewBox"), "{}", d.id);
            assert!(!svg.contains(" width=\"1em\""), "{} keeps a fixed size", d.id);
            // Ids are scoped to the agent, so icons can share a page.
            for (i, _) in svg.match_indices(" id=\"") {
                assert!(svg[i + 5..].starts_with(&format!("agl-{}-", d.id)), "{} has an unscoped id", d.id);
            }
        }
        assert_eq!(builtin("goose"), None);
    }

    #[test]
    fn custom_icons_must_be_plain_svg() {
        let ok = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/></svg>"#;
        assert_eq!(for_agent("goose", Some(ok)).as_deref(), Some(ok));
        assert_eq!(for_agent("goose", None), None);
        for bad in [
            r#"<svg><script>alert(1)</script></svg>"#,
            r#"<svg onload="alert(1)"></svg>"#,
            r#"<svg><a href="javascript:alert(1)"/></svg>"#,
            r#"<svg><foreignObject><div/></foreignObject></svg>"#,
            r#"<svg><image href="https://tracker.example/x.png"/></svg>"#,
            r#"<img src=x onerror=alert(1)>"#,
            "goose.png",
        ] {
            assert_eq!(for_agent("goose", Some(bad)), None, "{bad}");
        }
        // A custom agent replacing a built-in keeps that icon unless it names a valid one.
        assert_eq!(for_agent("pi", None).as_deref(), builtin("pi"));
        assert_eq!(for_agent("pi", Some("<svg onclick=\"x\"></svg>")).as_deref(), builtin("pi"));
        assert!(!has_event_handler(r#"<svg viewbox="0 0 1 1"><path d="m0 0" fill="none"/></svg>"#));
    }
}
