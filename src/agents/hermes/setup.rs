use crate::{setup::{Edit, Plan, SetupCtx}, Result};
use std::path::PathBuf;

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let home = crate::paths::absolute(&hermes_home(ctx))?;
    let plugin = home.join("plugins/herdr-respan");
    let source = include_str!("assets/herdr-respan/__init__.py")
        .replace("__HERDR_RESPAN_BIN__", &serde_json::to_string(&ctx.binary.to_string_lossy())?)
        .replace("__HERDR_RESPAN_STATE__", &serde_json::to_string(&ctx.state_dir.to_string_lossy())?)
        .replace("__HERDR_RESPAN_CONFIG__", &serde_json::to_string(&ctx.config_dir.to_string_lossy())?);
    Ok(Plan {
        edits: vec![
            Edit::WriteFile { path: plugin.join("plugin.yaml"), contents: include_bytes!("assets/herdr-respan/plugin.yaml").to_vec(), id: "herdr-respan-hermes-manifest".into() },
            Edit::WriteFile { path: plugin.join("__init__.py"), contents: source.into_bytes(), id: "herdr-respan-hermes-observers".into() },
            Edit::Text { file: home.join("config.yaml"), id: "herdr-respan-hermes-enabled".into(), apply: enable, revert: disable },
        ],
        notices: vec!["Hermes 0.18.0 supports turns, request usage, tools and children; auxiliary spans require a version with auxiliary observers (verified schema: 0.21.5). Gateway/Discord sessions are excluded. plugins.disabled wins over enabled; remove a pre-existing herdr-respan disabled entry yourself if applicable.".into()],
        ..Default::default()
    })
}

fn hermes_home(ctx: &SetupCtx) -> PathBuf {
    if let Ok(value) = std::env::var("HERMES_HOME") {
        let value = value.trim();
        if !value.is_empty() {
            return if value == "~" { ctx.home.clone() } else if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) { ctx.home.join(rest) } else { PathBuf::from(value) };
        }
    }
    #[cfg(windows)] {
        if let (Some(home), Some(profile)) = (std::env::var_os("HOME"), std::env::var_os("USERPROFILE")) {
            if home != profile { return PathBuf::from(home).join(".hermes"); }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|s| !s.is_empty()) { return PathBuf::from(local).join("hermes"); }
    }
    ctx.home.join(".hermes")
}

// Like herdr's installer, edit only the enabled list; never serialize the YAML tree.
// Unsupported YAML constructs fail explicitly instead of replacing user configuration.
const CREATED: &str = "# herdr-respan: created";

fn content(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped { escaped = false; continue; }
        if quote == Some('"') && c == '\\' { escaped = true; continue; }
        if let Some(q) = quote { if c == q { quote = None; } }
        else if c == '\'' || c == '"' { quote = Some(c); }
        else if c == '#' && (i == 0 || line.as_bytes()[i - 1].is_ascii_whitespace()) { return &line[..i]; }
    }
    line
}
fn indent(line: &str) -> usize { line.len() - line.trim_start_matches(' ').len() }
fn scalar(value: &str) -> &str { value.trim().trim_matches(['\'', '"']) }
fn boundary(lines: &[String], start: usize, depth: usize) -> usize {
    (start + 1..lines.len()).find(|&i| !content(&lines[i]).trim().is_empty() && indent(&lines[i]) <= depth).unwrap_or(lines.len())
}
fn key(line: &str, name: &str) -> bool { content(line).trim_start().strip_prefix(name).is_some_and(|s| s.starts_with(':')) }
type FlowParts = (usize, usize, Vec<(usize, usize)>);
fn flow_parts(line: &str) -> Result<FlowParts> {
    let open = line.find('[').ok_or("plugins.enabled must be a list")?;
    let mut quote = None; let mut escaped = false; let mut start = open + 1; let mut parts = Vec::new();
    for (i, c) in line.char_indices().filter(|(i, _)| *i > open) {
        if escaped { escaped = false; continue; }
        if quote == Some('"') && c == '\\' { escaped = true; continue; }
        if let Some(q) = quote { if c == q { quote = None; } continue; }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' | '{' | '}' => return Err("plugins.enabled requires scalar plugin names".into()),
            ',' => { parts.push((start, i)); start = i + 1; },
            ']' => { parts.push((start, i)); return Ok((open, i, parts)); },
            _ => {},
        }
    }
    Err("unterminated plugins.enabled flow list".into())
}

pub fn enable(text: &str) -> Result<String> { edit(text, true) }
pub fn disable(text: &str) -> Result<String> { edit(text, false) }
fn edit(text: &str, add: bool) -> Result<String> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let plugins: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| indent(l) == 0 && key(l, "plugins")).map(|(i, _)| i).collect();
    if plugins.len() > 1 { return Err("duplicate plugins mapping".into()); }
    let Some(&p) = plugins.first() else {
        if !add { return Ok(text.into()); }
        lines.extend([format!("plugins: {CREATED}"), format!("  enabled: {CREATED}"), "    - herdr-respan".into()]);
        return Ok(lines.join(newline) + if trailing || text.is_empty() { newline } else { "" });
    };
    if content(&lines[p]).trim() != "plugins:" { return Err("plugins must be a block mapping; flow mappings, aliases and legacy flat lists are not safely editable".into()); }
    let end = boundary(&lines, p, 0);
    let enabled: Vec<usize> = (p + 1..end).filter(|&i| indent(&lines[i]) == 2 && key(&lines[i], "enabled")).collect();
    if enabled.len() > 1 { return Err("duplicate plugins.enabled key".into()); }
    let Some(&e) = enabled.first() else {
        if !add { return Ok(text.into()); }
        if (p + 1..end).any(|i| content(&lines[i]).trim_start().starts_with('-')) { return Err("legacy plugins list is not a plugins.enabled mapping".into()); }
        lines.splice(p + 1..p + 1, [format!("  enabled: {CREATED}"), "    - herdr-respan".into()]);
        return Ok(lines.join(newline) + if trailing { newline } else { "" });
    };
    let tail = content(&lines[e]).trim().strip_prefix("enabled:").unwrap().trim();
    if tail.starts_with('[') {
        let (open, close, parts) = flow_parts(content(&lines[e]))?;
        let ours = parts.iter().position(|&(s, t)| scalar(&lines[e][s..t]) == "herdr-respan");
        if add {
            if ours.is_some() { return Ok(text.into()); }
            // Insert ahead of trailing whitespace: remove can restore it verbatim.
            let raw = &lines[e][open + 1..close];
            let at = open + 1 + raw.trim_end().len();
            let insertion = if raw.trim().is_empty() { "herdr-respan" } else if raw.trim_end().ends_with(',') { " herdr-respan," } else { ", herdr-respan" };
            lines[e].insert_str(at, insertion);
        } else if let Some(n) = ours {
            let (s, t) = parts[n];
            let start = if n > 0 { s - 1 } else { s };
            let stop = if n == 0 && n + 1 < parts.len() { t + 1 } else { t };
            // Preserve whitespace that preceded our inserted final scalar.
            let end = if n + 1 == parts.len() { t - (lines[e][s..t].len() - lines[e][s..t].trim_end().len()) } else { stop };
            lines[e].replace_range(start..end, "");
        } else { return Ok(text.into()); }
    } else {
        if !tail.is_empty() { return Err("plugins.enabled must be a block or flow list".into()); }
        let list_end = boundary(&lines, e, 2);
        let mut ours = Vec::new();
        for (i, line) in lines.iter().enumerate().take(list_end).skip(e + 1) {
            let item = content(line).trim();
            if item.is_empty() { continue; }
            let value = item.strip_prefix("- ").ok_or("plugins.enabled requires scalar list entries")?;
            if scalar(value) == "herdr-respan" { ours.push(i); }
        }
        if add {
            if !ours.is_empty() { return Ok(text.into()); }
            let insertion = (e + 1..list_end).rev().find(|&i| content(&lines[i]).trim().starts_with("- ")).map_or(e + 1, |i| i + 1);
            let item = if (e + 1..list_end).all(|i| content(&lines[i]).trim().is_empty()) && !lines[e].contains(CREATED) { "    - herdr-respan # herdr-respan: was-null" } else { "    - herdr-respan" };
            lines.insert(insertion, item.into());
        } else {
            if ours.is_empty() { return Ok(text.into()); }
            let was_null = ours.iter().any(|&i| lines[i].contains("# herdr-respan: was-null"));
            for i in ours.into_iter().rev() { lines.remove(i); }
            let empty = (e + 1..boundary(&lines, e, 2)).all(|i| content(&lines[i]).trim().is_empty());
            if empty && lines[e].contains(CREATED) { lines.remove(e); }
            else if empty && !was_null { lines[e] = lines[e].replacen("enabled:", "enabled: []", 1); }
            if lines[p].contains(CREATED) && (p + 1..boundary(&lines, p, 0)).all(|i| content(&lines[i]).trim().is_empty()) { lines.remove(p); }
        }
    }
    Ok(lines.join(newline) + if trailing && !lines.is_empty() { newline } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comment_preserving_roundtrip() {
        for original in ["", "# header\nmodel: example\n", "# header\r\nplugins:\r\n  enabled:\r\n    # user note\r\n    - herdr-agent-state # retained\r\n  disabled: [other]\r\n", "plugins:\n  enabled: [herdr-agent-state, 'comma,name'] # retain\n  disabled: []\n", "plugins:\n  enabled: [] # empty\n", "plugins:\n  enabled: # originally null\n    # keep comment\n", "plugins:\n  enabled: [ hermes-other ] # spacing\n", "plugins:\n  disabled: [] # retained\n"] {
            let added = enable(original).unwrap();
            assert_eq!(enable(&added).unwrap(), added);
            assert_eq!(disable(&added).unwrap(), original);
        }
    }
    #[test]
    fn foreign_entries_and_unsupported_shapes() {
        let before = "plugins:\n  enabled:\n    - herdr-agent-state\n";
        let added = enable(before).unwrap() + "    - foreign # added afterwards\n";
        let removed = disable(&added).unwrap();
        assert!(removed.contains("foreign # added afterwards"));
        assert!(removed.contains("herdr-agent-state"));
        assert!(!removed.contains("herdr-respan"));
        for bad in ["plugins: {enabled: []}\n", "plugins: [old]\n", "plugins:\n  enabled: true\n", "plugins:\n  enabled: *alias\n", "plugins:\n  - legacy\n"] { assert!(enable(bad).is_err()); }
        let existing = "plugins:\n  enabled: ['herdr-respan'] # already present\n";
        assert_eq!(enable(existing).unwrap(), existing);
    }
    #[test]
    fn actual_setup_remove_preserves_config_and_assets() {
        let tmp = crate::tests::Temp::new();
        let ctx = SetupCtx { home: tmp.path.join("home"), state_dir: tmp.path.join("state"), config_dir: tmp.path.join("config"), project: None, binary: tmp.path.join("state/bin/herdr-respan") };
        // Apply the actual asset/Text edits only to a disposable home, independent of HERMES_HOME.
        let mut plan = plan(&ctx).unwrap();
        let root = tmp.path.join("hermes");
        for edit in &mut plan.edits {
            match edit {
                Edit::WriteFile { path, .. } => *path = root.join("plugins/herdr-respan").join(path.file_name().unwrap()),
                Edit::Text { file, .. } => *file = root.join("config.yaml"),
                _ => unreachable!(),
            }
        }
        std::fs::create_dir_all(&root).unwrap();
        let original = "# user's comment\nplugins:\n  enabled: [herdr-agent-state] # keep\n";
        std::fs::write(root.join("config.yaml"), original).unwrap();
        crate::setup::apply(&ctx, "hermes", &plan).unwrap();
        let asset = std::fs::read_to_string(root.join("plugins/herdr-respan/__init__.py")).unwrap();
        assert!(!asset.contains("__HERDR_RESPAN_BIN__"));
        crate::setup::remove(&ctx, &["hermes".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("config.yaml")).unwrap(), original);
        assert!(!root.join("plugins/herdr-respan/__init__.py").exists());
        assert!(!root.join("plugins/herdr-respan/plugin.yaml").exists());
    }
}
