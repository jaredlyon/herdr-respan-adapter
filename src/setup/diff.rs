use super::files::Files;
use similar::TextDiff;
pub fn render(files: &Files) -> String {
    let mut output = String::new();
    for (path, staged) in files {
        if staged.before == staged.after { continue; }
        let before = staged.before.as_deref().unwrap_or_default(); let after = staged.after.as_deref().unwrap_or_default();
        match (std::str::from_utf8(before), std::str::from_utf8(after)) {
            (Ok(before), Ok(after)) => output.push_str(&TextDiff::from_lines(before, after).unified_diff().context_radius(3).header(&format!("a/{}", path.display()), &format!("b/{}", path.display())).to_string()),
            _ => output.push_str(&format!("Binary file {}: {} → {} bytes\n", path.display(), before.len(), after.len())),
        }
    }
    output
}
