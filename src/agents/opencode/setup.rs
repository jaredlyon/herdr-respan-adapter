use crate::{setup::{Edit, Plan, SetupCtx}, Result};
use serde_json::Value;
use std::path::PathBuf;

const TUI: &str = "./herdr-respan-tui.js";
const V2: &str = "./herdr-respan";
const BEGIN: &str = "/* herdr-respan registration begin */";
const END: &str = "/* herdr-respan registration end */";
const PROPERTY_END: &str = "/* herdr-respan property end */";
const CREATED: &str = "// herdr-respan created config\n";

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    // Match herdr's opencode_dir(): it intentionally does not use XDG_CONFIG_HOME.
    let dir = ctx.project.as_ref().map_or_else(|| ctx.home.join(".config/opencode"), |p| p.join(".opencode"));
    let render = |asset: &str| -> Result<Vec<u8>> {
        Ok(asset.replace("__HERDR_RESPAN_BIN__", &serde_json::to_string(&ctx.binary.to_string_lossy())?)
            .replace("__HERDR_RESPAN_STATE__", &serde_json::to_string(&ctx.state_dir.to_string_lossy())?)
            .replace("__HERDR_RESPAN_CONFIG__", &serde_json::to_string(&ctx.config_dir.to_string_lossy())?).into_bytes())
    };
    let mut plan = Plan::default();
    for (name, asset) in [
        ("plugins/herdr-respan.js", include_str!("assets/server.js")),
        ("herdr-respan/bridge.js", include_str!("assets/bridge.js")),
        ("herdr-respan-tui.js", include_str!("assets/tui.js")),
        ("herdr-respan/tui.js", include_str!("assets/v2-tui.js")),
    ] {
        plan.edits.push(Edit::WriteFile { path: dir.join(name), contents: render(asset)?, id: format!("herdr-respan-opencode-{name}") });
    }
    let tui = if dir.join("tui.jsonc").exists() || !dir.join("tui.json").exists() { "tui.jsonc" } else { "tui.json" };
    plan.edits.push(Edit::Text { file: dir.join(tui), id: "herdr-respan-opencode-tui-registration".into(), apply: add_tui, revert: remove_registration });
    let state = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| ctx.home.join(".local/state")).join("opencode");
    if !dir.join("cli.json").exists() && (dir.join("tui.json").exists() || state.join("kv.json").exists()) {
        plan.notices.push("OpenCode V2 cli.json registration deferred to preserve its V1 preference migration. Start V2 once, then rerun setup opencode.".into());
    } else {
        plan.edits.push(Edit::Text { file: dir.join("cli.json"), id: "herdr-respan-opencode-v2-registration".into(), apply: add_cli, revert: remove_registration });
    }
    plan.notices.push("OpenCode V1 requires >= 1.18.29; V2 TUI API follows herdr integration v13 (beta 19242). Restart OpenCode after setup. Local plugin files auto-load: no opencode.json plugin entry is added.".into());
    Ok(plan)
}

fn add_tui(text: &str) -> Result<String> { add_registration(text, "plugin", TUI) }
fn add_cli(text: &str) -> Result<String> { add_registration(text, "plugins", V2) }

// Mask comments and trailing commas without shifting byte offsets. serde validates
// the entire JSONC value before the narrow, marker-owned insertion below.
fn jsonc(text: &str) -> Result<(Value, Vec<u8>)> {
    let bytes = text.as_bytes();
    let mut clean = bytes.to_vec();
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        if quoted {
            if bytes[i] == b'\\' { i += 2; continue; }
            if bytes[i] == b'"' { quoted = false; }
        } else if bytes[i] == b'"' { quoted = true; }
        else if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' { clean[i] = b' '; i += 1; }
            continue;
        } else if bytes[i..].starts_with(b"/*") {
            let start = i; i += 2;
            while i + 1 < bytes.len() && !bytes[i..].starts_with(b"*/") { i += 1; }
            if i + 1 >= bytes.len() { return Err("unterminated OpenCode JSONC comment".into()); }
            i += 2; clean[start..i].fill(b' '); continue;
        }
        i += 1;
    }
    quoted = false; i = 0;
    while i < clean.len() {
        if quoted {
            if clean[i] == b'\\' { i += 2; continue; }
            if clean[i] == b'"' { quoted = false; }
        } else if clean[i] == b'"' { quoted = true; }
        else if clean[i] == b',' {
            let next = clean[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) { clean[i] = b' '; }
        }
        i += 1;
    }
    let value: Value = serde_json::from_slice(&clean)?;
    if !value.is_object() { return Err("OpenCode config must be an object".into()); }
    Ok((value, clean))
}

fn add_registration(text: &str, key: &str, entry: &str) -> Result<String> {
    let created = text.is_empty();
    let source = if created { "{}\n" } else { text };
    let (value, clean) = jsonc(source)?;
    let array = value.get(key).map(|v| v.as_array().ok_or("OpenCode plugin list must be an array")).transpose()?;
    if array.is_some_and(|a| a.iter().any(|v| v.as_str() == Some(entry) || v.get("package").and_then(Value::as_str) == Some(entry)
        || v.as_array().and_then(|a| a.first()).and_then(Value::as_str) == Some(entry))) { return Ok(text.into()); }
    let mut pos = clean.iter().position(|b| *b == b'{').ok_or("OpenCode object missing")? + 1;
    let mut snippet = serde_json::to_string(entry)?;
    let nonempty;
    if let Some(array) = array {
        // Only inspect top-level property names; strings and nested objects may
        // legitimately contain another "plugin" key.
        let mut depth = 0usize; let mut i = 0; let mut found = false;
        while i < clean.len() {
            match clean[i] {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth -= 1,
                b'"' => {
                    let start = i; i += 1;
                    while i < clean.len() && clean[i] != b'"' { if clean[i] == b'\\' { i += 1; } i += 1; }
                    if depth == 1 && serde_json::from_slice::<String>(&clean[start..=i])? == key {
                        let mut next = i + 1;
                        while clean[next].is_ascii_whitespace() { next += 1; }
                        if clean[next] == b':' {
                            next += 1; while clean[next].is_ascii_whitespace() { next += 1; }
                            if clean[next] != b'[' { return Err("OpenCode plugin list must be an array".into()); }
                            pos = next + 1; found = true; break;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if !found { return Err("OpenCode plugin list location missing".into()); }
        snippet = serde_json::to_string(entry)?;
        nonempty = !array.is_empty();
    } else { nonempty = !value.as_object().expect("validated object").is_empty(); }
    if nonempty { snippet.push(','); }
    let insert = if array.is_some() {
        format!("{BEGIN}{snippet}{END}")
    } else {
        let separator = if nonempty { "," } else { "" };
        format!("/* herdr-respan created {key} begin */{}:[{BEGIN}{}{END}]{separator}{PROPERTY_END}",
            serde_json::to_string(key)?, serde_json::to_string(entry)?)
    };
    let mut output = source.to_string(); output.insert_str(pos, &insert);
    if created { output.insert_str(0, CREATED); }
    jsonc(&output)?;
    Ok(output)
}

fn remove_registration(text: &str) -> Result<String> {
    let (_, clean) = jsonc(text)?;
    let Some(start) = text.find(BEGIN) else { return Ok(text.into()); };
    let end = text[start..].find(END).ok_or("OpenCode registration end marker missing")? + start + END.len();
    let entry = text[start + BEGIN.len()..end - END.len()].trim();
    let owned_comma = entry.ends_with(',');
    let entry: String = serde_json::from_str(entry.trim_end_matches(','))?;
    if entry != TUI && entry != V2 { return Err("OpenCode owned registration was modified".into()); }
    let mut output = text.to_string();
    // A user may append/prepend foreign elements to a list that we created.
    // Remove only our element and one adjacent comma, never the list contents.
    if !owned_comma {
        let after = clean[end..].iter().position(|b| !b.is_ascii_whitespace()).map(|i| end + i);
        if let Some(comma) = after.filter(|i| clean[*i] == b',') {
            output.remove(comma);
        } else if let Some(comma) = clean[..start].iter().rposition(|b| !b.is_ascii_whitespace()).filter(|i| clean[*i] == b',') {
            output.remove(comma);
            output.replace_range(start - 1..end - 1, "");
            return finish_removal(output);
        }
    }
    output.replace_range(start..end, "");
    finish_removal(output)
}

fn finish_removal(mut output: String) -> Result<String> {
    let (value, _) = jsonc(&output)?;
    for key in ["plugin", "plugins"] {
        let marker = format!("/* herdr-respan created {key} begin */");
        let Some(start) = output.find(&marker) else { continue; };
        let end = output[start..].find(PROPERTY_END).ok_or("OpenCode property end marker missing")? + start + PROPERTY_END.len();
        let body = &output[start + marker.len()..end - PROPERTY_END.len()];
        let (property, _) = jsonc(&format!("{{{body}}}"))?;
        if value.get(key).and_then(Value::as_array).is_some_and(|a| a.is_empty())
            && property.as_object().is_some_and(|o| o.len() == 1) {
            output.replace_range(start..end, "");
        } else {
            // Once foreign content exists, the property belongs to the user.
            output.replace_range(end - PROPERTY_END.len()..end, "");
            output.replace_range(start..start + marker.len(), "");
        }
    }
    let (value, _) = jsonc(&output)?;
    if output.starts_with(CREATED) && value.as_object().is_some_and(|o| o.is_empty()) { return Ok(String::new()); }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{paths::Paths, setup};

    #[test]
    fn jsonc_registration_preserves_comments_strings_and_foreign_entries() {
        for original in ["{}", "{\"plugin\":[]}", "// config\n{\"theme\":\"a // b\",\"nested\":{\"plugin\":[\"x\"]},\"plugin\":[\"./herdr-tui-session.js\", /* keep */ [\"other\",{}],],}\n"] {
            let result = add_tui(original).unwrap();
            assert_eq!(add_tui(&result).unwrap(), result);
            assert_eq!(remove_registration(&result).unwrap(), original);
            assert_eq!(jsonc(&result).unwrap().0["plugin"][0], TUI);
        }
        assert_eq!(remove_registration(&add_tui("").unwrap()).unwrap(), "");
        assert!(add_tui("{\"plugin\":false}").is_err());
        assert!(add_tui("{ /* broken ").is_err());
        let foreign = "{\"plugin\":[{\"package\":\"./herdr-respan-tui.js\"}]}";
        assert_eq!(add_tui(foreign).unwrap(), foreign);
    }

    #[test]
    fn remove_preserves_foreign_plugins_appended_to_created_arrays() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
        let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(tmp.path.join("repo"))).unwrap();
        let root = ctx.project.as_ref().unwrap().join(".opencode");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("tui.jsonc"), "// user preference\n{\"theme\":\"system\"}\n").unwrap();
        setup::apply(&ctx, "opencode", &plan(&ctx).unwrap()).unwrap();
        for file in ["tui.jsonc", "cli.json"] {
            let installed = std::fs::read_to_string(root.join(file)).unwrap();
            let edited = installed.replacen(&format!("{END}]"), &format!("{END},\"foreign-plugin\"]"), 1);
            assert_ne!(installed, edited);
            std::fs::write(root.join(file), edited).unwrap();
        }
        setup::remove(&ctx, &["opencode".into()]).unwrap();
        for (file, key) in [("tui.jsonc", "plugin"), ("cli.json", "plugins")] {
            let remaining = std::fs::read_to_string(root.join(file)).unwrap();
            let (value, _) = jsonc(&remaining).unwrap();
            assert_eq!(value[key], serde_json::json!(["foreign-plugin"]));
            assert!(!remaining.contains(BEGIN));
            assert!(!remaining.contains(PROPERTY_END));
            if file == "tui.jsonc" { assert_eq!(value["theme"], "system"); assert!(remaining.contains("// user preference")); }
        }
        assert!(!root.join("plugins/herdr-respan.js").exists());
    }

    #[test]
    fn setup_owned_files_roundtrip_and_migration_deferral() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths { state: tmp.path.join("state with spaces"), config: tmp.path.join("config") };
        let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(tmp.path.join("repo"))).unwrap();
        let root = ctx.project.as_ref().unwrap().join(".opencode");
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        let original = "// herdr's entry\n{\"plugin\":[\"./herdr-tui-session.js\",],\"theme\":\"system\"}\n";
        std::fs::write(root.join("tui.jsonc"), original).unwrap();
        let herdr = b"// managed by herdr\n";
        std::fs::write(root.join("plugins/herdr-agent-state.js"), herdr).unwrap();
        let edits = plan(&ctx).unwrap();
        setup::apply(&ctx, "opencode", &edits).unwrap();
        setup::apply(&ctx, "opencode", &edits).unwrap();
        let rendered = std::fs::read_to_string(root.join("herdr-respan/bridge.js")).unwrap();
        assert!(!rendered.contains("__HERDR_RESPAN_"));
        assert!(rendered.contains("state with spaces"));
        setup::remove(&ctx, &["opencode".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("tui.jsonc")).unwrap(), original);
        assert_eq!(std::fs::read(root.join("plugins/herdr-agent-state.js")).unwrap(), herdr);
        assert!(!root.join("cli.json").exists());
        assert!(!root.join("plugins/herdr-respan.js").exists());
        std::fs::write(root.join("tui.json"), "{\"theme\":\"old\"}").unwrap();
        let deferred = plan(&ctx).unwrap();
        assert!(deferred.notices.iter().any(|s| s.contains("deferred")));
        assert!(!deferred.edits.iter().any(|e| matches!(e, Edit::Text {file,..} if file.ends_with("cli.json"))));
        std::fs::write(root.join("cli.json"), "{\"plugins\":[\"./herdr-opencode\"]}").unwrap();
        assert!(plan(&ctx).unwrap().edits.iter().any(|e| matches!(e, Edit::Text {file,..} if file.ends_with("cli.json"))));
    }
}
