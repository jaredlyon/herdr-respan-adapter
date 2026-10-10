pub mod json;
pub mod toml;
mod files;
mod owned;
mod diff;
pub mod detect;
use crate::{paths::Paths, Result};
use owned::{Change, Entry, Owned};
use std::path::{Path, PathBuf};

pub struct SetupCtx {
    pub home: PathBuf, pub config_dir: PathBuf, pub state_dir: PathBuf,
    pub project: Option<PathBuf>, pub binary: PathBuf,
}
impl SetupCtx {
    pub fn new(paths: &Paths, home: PathBuf, project: Option<PathBuf>) -> Result<Self> {
        Ok(Self { home: crate::paths::absolute(&home)?, config_dir: paths.config.clone(), state_dir: paths.state.clone(),
            project: project.map(|p| crate::paths::absolute(&p)).transpose()?, binary: paths.binary() })
    }
    pub fn agent_home(&self, variable: &str, default: &str) -> PathBuf {
        std::env::var_os(variable).map(PathBuf::from).unwrap_or_else(|| self.home.join(default))
    }
    pub fn hook_command(&self, kind: &str) -> String {
        crate::agents::common::hookcmd::command(self, kind, if cfg!(windows) { crate::agents::common::hookcmd::Shell::Cmd } else { crate::agents::common::hookcmd::Shell::Posix })
    }
}
#[derive(Default)]
pub struct Plan { pub edits: Vec<Edit>, pub notices: Vec<String>, pub offers: Vec<Offer> }
pub struct Offer { pub id: &'static str, pub prompt: String, pub edits: Vec<Edit> }
pub enum Edit {
    JsonInsert { file: PathBuf, pointer: String, value: serde_json::Value, id: String },
    /// Own a scalar/object member only when absent; never replace foreign values.
    JsonSetIfAbsent { file: PathBuf, pointer: String, value: serde_json::Value, id: String },
    TomlInsert { file: PathBuf, path: Vec<String>, value: toml_edit::Item, id: String },
    WriteFile { path: PathBuf, contents: Vec<u8>, id: String },
    Text { file: PathBuf, id: String, apply: fn(&str) -> Result<String>, revert: fn(&str) -> Result<String> },
    /// The non-secret argument is persisted so removal uses the original installation context.
    TextArg { file: PathBuf, id: String, arg: String, apply: fn(&str,&str) -> Result<String>, revert: fn(&str,&str) -> Result<String> },
}
impl Edit {
    fn file_id(&self) -> (&Path, &str) { match self { Self::JsonInsert { file,id,.. } | Self::JsonSetIfAbsent { file,id,.. } | Self::TextArg { file,id,.. } | Self::TomlInsert { file,id,.. } | Self::Text { file,id,.. } => (file,id), Self::WriteFile { path,id,.. } => (path,id) } }
}

fn transaction_lock(state: &Path) -> Result<crate::spool::Lock> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(lock) = crate::spool::Lock::acquire_at(&state.join("setup.lock"), std::time::Duration::from_secs(60))? { return Ok(lock); }
        if std::time::Instant::now() >= deadline { return Err("timed out waiting for setup transaction".into()); }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn validate_file(ctx: &SetupCtx, file: &Path, id: &str) -> Result<()> {
    if !file.is_absolute() || !id.contains("herdr-respan") { return Err("setup requires absolute paths and herdr-respan ownership markers".into()); }
    if file.file_name().is_some_and(|s| s == "auth.json" || s == "credentials.json" || s == ".env") || file.starts_with(ctx.home.join(".respan")) { return Err("setup may not edit credential stores".into()); }
    Ok(())
}
type TextFn = fn(&str) -> Result<String>;
type TextArgFn = fn(&str, &str) -> Result<String>;
enum Callback { Text(TextFn, TextFn), Arg(TextArgFn, TextArgFn) }
fn find_callback(entry: &Entry, plan: &Plan) -> Result<Option<Callback>> {
    let matches: Vec<_> = plan.edits.iter().chain(plan.offers.iter().flat_map(|o| &o.edits)).filter(|e| {
        e.file_id().1 == entry.id && matches!((e, &entry.change), (Edit::Text {..}, Change::Text) | (Edit::TextArg {..}, Change::TextArg {..}))
    }).collect();
    let edit = matches.iter().find(|e| e.file_id().0 == entry.file).copied().or(if matches.len() == 1 { matches.first().copied() } else { None });
    match edit {
        Some(Edit::Text {apply, revert, ..}) => Ok(Some(Callback::Text(*apply, *revert))),
        Some(Edit::TextArg {apply, revert, ..}) => Ok(Some(Callback::Arg(*apply, *revert))),
        _ if !matches.is_empty() => Err("ambiguous owned text callback".into()),
        _ => Ok(None),
    }
}
fn text_transform(ctx: &SetupCtx, entry: &Entry, hint: Option<(&str, &Plan)>, current: &str, install: bool) -> Result<String> {
    let mut callback = None;
    if let Some((agent, plan)) = hint { if agent == entry.agent { callback = find_callback(entry, plan)?; } }
    if callback.is_none() {
        let spec = crate::agents::find(&entry.agent).ok_or("text adapter no longer available")?;
        callback = find_callback(entry, &(spec.plan)(ctx)?)?;
    }
    match (callback.ok_or("missing owned text callback")?, &entry.change) {
        (Callback::Text(apply, revert), Change::Text) => if install { apply(current) } else { revert(current) },
        (Callback::Arg(apply, revert), Change::TextArg {arg}) => if install { apply(current, arg) } else { revert(current, arg) },
        _ => Err("owned text callback type changed".into()),
    }
}
fn install_entry(ctx: &SetupCtx, entry: &mut Entry, previous: Option<&Entry>, files: &mut files::Files, hint: Option<(&str, &Plan)>) -> Result<()> {
    validate_file(ctx, &entry.file, &entry.id)?;
    let staged = files::get(files, &entry.file)?;
    match &mut entry.change {
        Change::Json {pointer, value, created} => {
            let mut root = match &staged.after { Some(b) => serde_json::from_slice(b)?, None => serde_json::json!({}) };
            let (added, extra) = json::insert(&mut root, pointer, value)?;
            created.extend(extra);
            if added { staged.after = Some(serde_json::to_vec_pretty(&root)?); }
        }
        Change::JsonSet {pointer, value, created} => {
            let mut root = match &staged.after { Some(b) => serde_json::from_slice(b)?, None => serde_json::json!({}) };
            if root.pointer(pointer).is_some_and(|v| v != &*value) { return Err("owned JSON value was edited externally".into()); }
            let (added, extra) = json::set_if_absent(&mut root, pointer, value)?;
            created.extend(extra);
            if added { staged.after = Some(serde_json::to_vec_pretty(&root)?); }
        }
        Change::Toml {path, value, created, ..} => {
            let mut doc: toml_edit::DocumentMut = std::str::from_utf8(staged.after.as_deref().unwrap_or_default())?.parse()?;
            let (added, _, extra) = toml::restore(&mut doc, path, &toml::decode(value)?)?;
            created.extend(extra);
            if added { staged.after = Some(doc.to_string().into_bytes()); }
        }
        Change::File {contents} => {
            let old = previous.and_then(|e| if let Change::File {contents} = &e.change { Some(contents) } else { None });
            if staged.after.as_ref().is_some_and(|b| b != &*contents && Some(b) != old) { return Err("owned asset was edited externally; refusing overwrite".into()); }
            staged.after = Some(contents.clone());
        }
        Change::Text | Change::TextArg {..} => {
            let before = std::str::from_utf8(staged.after.as_deref().unwrap_or_default())?;
            let after = text_transform(ctx, entry, hint, before, true)?;
            if !after.contains("herdr-respan") || text_transform(ctx, entry, hint, &after, false)? == after { return Err("cannot identify installed owned text entry".into()); }
            if before != after { staged.after = Some(after.into_bytes()); }
        }
    }
    Ok(())
}
fn remove_entry(ctx: &SetupCtx, entry: &Entry, files: &mut files::Files, hint: Option<(&str, &Plan)>) -> Result<()> {
    validate_file(ctx, &entry.file, &entry.id)?;
    let staged = files::get(files, &entry.file)?;
    let Some(bytes) = &staged.after else { return Ok(()); };
    match &entry.change {
        Change::Json {pointer, value, created} | Change::JsonSet {pointer, value, created} => {
            let mut root: serde_json::Value = serde_json::from_slice(bytes)?;
            if matches!(entry.change, Change::Json {..}) { json::remove(&mut root, pointer, value, created)?; }
            else { json::remove_set(&mut root, pointer, value, created)?; }
            staged.after = if !entry.existed && root.as_object().is_some_and(|o| o.is_empty()) { None } else { Some(serde_json::to_vec_pretty(&root)?) };
        }
        Change::Toml {path, value, appended, created} => {
            let mut doc: toml_edit::DocumentMut = std::str::from_utf8(bytes)?.parse()?;
            toml::remove(&mut doc, path, &toml::decode(value)?, *appended, created)?;
            staged.after = if !entry.existed && doc.as_table().is_empty() { None } else { Some(doc.to_string().into_bytes()) };
        }
        Change::File {contents} => {
            if bytes != contents { return Err("owned asset modified; refusing removal".into()); }
            staged.after = None;
        }
        Change::Text | Change::TextArg {..} => {
            let reverted = text_transform(ctx, entry, hint, std::str::from_utf8(bytes)?, false)?;
            staged.after = if !entry.existed && reverted.is_empty() { None } else { Some(reverted.into_bytes()) };
        }
    }
    Ok(())
}
fn recover_stage(ctx: &SetupCtx, hint: Option<(&str, &Plan)>) -> Result<(files::Files, Owned)> {
    let mut owned = Owned::load(&ctx.state_dir)?;
    let mut files = files::Files::new();
    // Old versions journaled a union in entries. Only collapse duplicate asset versions
    // when current bytes identify one version; missing/modified duplicates are ambiguous.
    let mut normalized: Vec<Entry> = Vec::new();
    for entry in &owned.entries {
        if let Some(i) = normalized.iter().position(|e| e.same_edit(entry)) {
            if normalized[i] == *entry { continue; }
            match (&normalized[i].change, &entry.change) {
                (Change::File {contents: a}, Change::File {contents: b}) => {
                    let current = files::get(&mut files, &entry.file)?.after.as_ref();
                    if current == Some(b) { normalized[i] = entry.clone(); }
                    else if current != Some(a) {
                        let installed_pending = owned.pending.as_ref().is_some_and(|p| p.install.iter().any(|e| e.same_edit(entry) && matches!(&e.change, Change::File {contents} if current == Some(contents))));
                        if !installed_pending { return Err("ambiguous legacy asset ownership journal".into()); }
                    }
                }
                _ => return Err("ambiguous legacy ownership journal".into()),
            }
        } else { normalized.push(entry.clone()); }
    }
    owned.entries = normalized;
    if let Some(pending) = owned.pending.take() {
        for entry in &pending.remove {
            // A remove followed by reinstall may already have reached its new asset.
            let replacement = pending.install.iter().find(|e| e.same_edit(entry));
            let already_replaced = if let Some(Entry {file, change: Change::File {contents}, ..}) = replacement {
                files::get(&mut files, file)?.after.as_ref() == Some(contents)
            } else { false };
            if !already_replaced { remove_entry(ctx, entry, &mut files, hint)?; }
            owned.record_remove(entry.clone());
        }
        for mut entry in pending.install {
            let previous = owned.entries.iter().find(|old| old.same_edit(&entry)).cloned();
            install_entry(ctx, &mut entry, previous.as_ref(), &mut files, hint)?;
            owned.record_install(entry);
        }
    }
    Ok((files, owned))
}

fn recover_locked(ctx: &SetupCtx, hint: Option<(&str, &Plan)>, lock: &crate::spool::Lock) -> Result<Owned> {
    let (files, mut owned) = recover_stage(ctx, hint)?;
    if owned.pending.is_some() {
        // Finish the already-confirmed transaction before journaling a different one.
        // Both commits stay under the same lock; this keeps install/remove replay unambiguous.
        files::commit(&files, &owned, &ctx.state_dir, lock)?;
        owned.pending = None;
    }
    Ok(owned)
}

fn stage(ctx: &SetupCtx, agent: &str, plan: &Plan, mut files: files::Files, mut owned: Owned) -> Result<(files::Files, Owned)> {
    for edit in &plan.edits {
        let (file, id) = edit.file_id();
        validate_file(ctx, file, id)?;
        let previous = owned.entries.iter().position(|e| e.agent == agent && e.id == id && e.file == file);
        if let Some(i) = previous {
            let old = owned.entries[i].clone();
            let mut entry = old.clone();
            if let (Edit::WriteFile {contents, ..}, Change::File {..}) = (edit, &entry.change) { entry.change = Change::File {contents: contents.clone()}; }
            install_entry(ctx, &mut entry, Some(&old), &mut files, Some((agent, plan)))?;
            owned.record_install(entry);
            continue;
        }
        let staged = files::get(&mut files, file)?;
        let existed = staged.before.is_some() && !owned.entries.iter().any(|e| e.file == file && !e.existed);
        let change = match edit {
            Edit::JsonInsert { pointer, value, .. } => {
                if !value.to_string().contains("herdr-respan") { return Err("JSON entry lacks ownership marker".into()); }
                let mut root = match &staged.after { Some(b) => serde_json::from_slice(b)?, None => serde_json::json!({}) };
                let (added, mut created) = json::insert(&mut root, pointer, value)?;
                if !added { continue; }
                for entry in &owned.entries { if entry.file == file { if let Change::Json { created: inherited, .. } | Change::JsonSet {created:inherited,..} = &entry.change { created.extend(inherited.iter().cloned()); } } }
                staged.after = Some(serde_json::to_vec_pretty(&root)?);
                Change::Json { pointer: pointer.clone(), value: value.clone(), created }
            }
            Edit::JsonSetIfAbsent { pointer,value,.. } => {
                let mut root=match &staged.after {Some(b)=>serde_json::from_slice(b)?,None=>serde_json::json!({})};
                let (added,mut created)=json::set_if_absent(&mut root,pointer,value)?;
                if !added {continue;}
                for entry in &owned.entries {if entry.file==file {match &entry.change {Change::Json {created:inherited,..}|Change::JsonSet {created:inherited,..}=>created.extend(inherited.iter().cloned()),_=>{}}}}
                staged.after=Some(serde_json::to_vec_pretty(&root)?);
                Change::JsonSet {pointer:pointer.clone(),value:value.clone(),created}
            }
            Edit::TomlInsert { path, value, .. } => {
                let mut doc: toml_edit::DocumentMut = std::str::from_utf8(staged.after.as_deref().unwrap_or_default())?.parse()?;
                let (added, appended, mut created) = toml::insert(&mut doc, path, value)?;
                if !added { continue; }
                for entry in &owned.entries { if entry.file == file { if let Change::Toml { created: inherited, .. } = &entry.change { created.extend(inherited.iter().cloned()); } } }
                staged.after = Some(doc.to_string().into_bytes());
                Change::Toml { path: path.clone(), value: toml::encode(value), appended, created }
            }
            Edit::WriteFile { contents, .. } => {
                if staged.after.as_ref() == Some(contents) { continue; }
                if staged.after.is_some() { return Err("refusing to overwrite foreign asset".into()); }
                staged.after = Some(contents.clone()); Change::File { contents: contents.clone() }
            }
            Edit::Text { apply, .. } => {
                let before = std::str::from_utf8(staged.after.as_deref().unwrap_or_default())?.to_string(); let after = apply(&before)?;
                if before == after { continue; }
                if !after.contains("herdr-respan") { return Err("text entry lacks ownership marker".into()); }
                staged.after = Some(after.as_bytes().to_vec()); Change::Text
            }
            Edit::TextArg {apply,arg,..} => {
                let before=std::str::from_utf8(staged.after.as_deref().unwrap_or_default())?;let after=apply(before,arg)?;
                if before==after {continue;}
                if !after.contains("herdr-respan") {return Err("text entry lacks ownership marker".into());}
                staged.after=Some(after.into_bytes());Change::TextArg {arg:arg.clone()}
            }
        };
        owned.record_install(Entry { agent: agent.into(), id: id.into(), file: file.into(), existed, change });
    }
    Ok((files, owned))
}
pub fn accept_offers(plan: &mut Plan, accepted: &[String], interactive: bool, mut ask: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    for offer in &mut plan.offers {
        if accepted.iter().any(|id| id == offer.id) || (interactive && ask(&offer.prompt)?) { plan.edits.append(&mut offer.edits); }
    }
    Ok(())
}
/// Preview recovery and the requested plan in memory only: no lock, journal, or config writes.
pub fn preview(ctx: &SetupCtx, agent: &str, plan: &Plan) -> Result<String> {
    let (files, owned) = recover_stage(ctx, Some((agent, plan)))?;
    let (files, _) = stage(ctx, agent, plan, files, owned)?;
    Ok(diff::render(&files))
}
/// Caller must obtain explicit user confirmation before applying the previewed plan.
pub fn apply(ctx: &SetupCtx, agent: &str, plan: &Plan) -> Result<()> {
    let lock = transaction_lock(&ctx.state_dir)?;
    apply_locked(ctx, agent, plan, &lock)
}
fn apply_locked(ctx: &SetupCtx, agent: &str, plan: &Plan, lock: &crate::spool::Lock) -> Result<()> {
    let owned = recover_locked(ctx, Some((agent, plan)), lock)?;
    let (files, owned) = stage(ctx, agent, plan, files::Files::new(), owned)?;
    files::commit(&files, &owned, &ctx.state_dir, lock)
}
pub fn remove(ctx: &SetupCtx, agents: &[String]) -> Result<()> {
    let lock = transaction_lock(&ctx.state_dir)?;
    let mut owned = recover_locked(ctx, None, &lock)?;
    let mut files = files::Files::new();
    let entries: Vec<_> = owned.entries.iter().rev().filter(|e| agents.is_empty() || agents.contains(&e.agent)).cloned().collect();
    for entry in entries {
        remove_entry(ctx, &entry, &mut files, None)?;
        owned.record_remove(entry);
    }
    files::commit(&files, &owned, &ctx.state_dir, &lock)
}
pub(crate) fn installed_agents(state:&Path)->Result<std::collections::BTreeSet<String>> {Ok(Owned::load(state)?.entries.into_iter().map(|e|e.agent).collect())}
pub fn installed(state: &Path, agent: &str) -> bool { Owned::load(state).is_ok_and(|o| o.entries.iter().any(|e| e.agent == agent)) }
/// Binary and version replacement share the setup transaction lock, never the ownership journal.
pub fn refresh_binary(paths: &Paths) -> Result<()> {
    let lock = transaction_lock(&paths.state)?;
    refresh_binary_locked(paths, &lock)
}
fn refresh_binary_locked(paths: &Paths, lock: &crate::spool::Lock) -> Result<()> {
    let current = std::env::current_exe()?; let binary = paths.binary();
    let version = paths.state.join("bin/version");
    if binary.exists() && std::fs::read_to_string(&version).ok().as_deref() == Some(env!("CARGO_PKG_VERSION")) { return Ok(()); }
    std::fs::create_dir_all(binary.parent().ok_or("missing binary parent")?)?;
    let suffix = format!("herdr-respan-{}-{}", std::process::id(), crate::record::now_ns());
    if current != binary {
        let tmp = binary.with_extension(&suffix);
        let result = (|| -> Result<()> {
            std::fs::copy(&current, &tmp)?;
            lock.refresh()?;
            std::fs::rename(&tmp, &binary)?;
            Ok(())
        })();
        if let Err(error) = result { let _ = std::fs::remove_file(&tmp); return Err(error); }
    }
    // Even when running the stable copy, repair a marker interrupted after binary replacement.
    let tmp = version.with_extension(&suffix);
    let result = (|| -> Result<()> {
        crate::paths::write_private(&tmp, env!("CARGO_PKG_VERSION").as_bytes())?;
        lock.refresh()?;
        std::fs::rename(&tmp, &version)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result
}
pub fn refresh_owned(ctx:&SetupCtx)->Result<()> {refresh_owned_agents(ctx,crate::agents::AGENTS)}
fn refresh_owned_agents(ctx:&SetupCtx,agents:&[crate::agents::AgentSpec])->Result<()> {
    let lock = transaction_lock(&ctx.state_dir)?;
    let mut owned = recover_locked(ctx, None, &lock)?;
    let mut files = files::Files::new();
    for agent in agents {
        if owned.entries.iter().any(|e|e.agent==agent.kind&&matches!(e.change,Change::File {..})) {
            let mut plan=match (agent.plan)(ctx) {Ok(plan)=>plan,Err(_)=>{crate::log::error(&ctx.state_dir,"owned asset refresh plan failed; skipping agent");continue;}};
            for offer in &mut plan.offers { plan.edits.append(&mut offer.edits); }
            plan.edits.retain(|e| matches!(e, Edit::WriteFile { path, id, .. } if owned.entries.iter().any(|o| o.agent == agent.kind && &o.file == path && &o.id == id)));
            (files, owned) = stage(ctx, agent.kind, &plan, files, owned)?;
        }
    }
    files::commit(&files, &owned, &ctx.state_dir, &lock)
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn foreign_json_toml_roundtrip() {
        let tmp = crate::tests::Temp::new(); let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") }; let ctx = SetupCtx::new(&paths, tmp.path.join("home"), None).unwrap();
        let json_file = tmp.path.join("hooks.json"); let toml_file = tmp.path.join("config.toml");
        let original = serde_json::json!({"hooks":{"Stop":[{"command":"herdr-agent-state"}]},"telemetry":{"enabled":true}});
        std::fs::write(&json_file, original.to_string()).unwrap();
        let text = "# preserved comment\n[[hooks]]\ncommand = 'herdr-agent-state'\n[features]\nother = true\n"; std::fs::write(&toml_file, text).unwrap();
        let mut doc: toml_edit::DocumentMut = "[[hooks]]\ncommand = 'herdr-respan hook fake'\n".parse().unwrap();
        let plan = Plan { edits: vec![Edit::JsonInsert { file: json_file.clone(), pointer:"/hooks/Stop".into(), value:serde_json::json!({"command":"herdr-respan hook fake"}), id:"herdr-respan-fake-json".into() },
            Edit::JsonInsert { file:json_file.clone(),pointer:"/new/nested".into(),value:serde_json::json!("herdr-respan"),id:"herdr-respan-new".into() },
            Edit::TomlInsert { file:toml_file.clone(),path:vec!["hooks".into()],value:doc.remove("hooks").unwrap(),id:"herdr-respan-fake-toml".into() },
            Edit::TomlInsert { file:toml_file.clone(),path:vec!["features".into(),"hooks".into()],value:toml_edit::value(true),id:"herdr-respan-flag".into() }], ..Default::default() };
        assert!(preview(&ctx,"fake",&plan).unwrap().contains("@@"));
        apply(&ctx,"fake",&plan).unwrap(); let once = std::fs::read(&json_file).unwrap(); apply(&ctx,"fake",&plan).unwrap(); assert_eq!(once,std::fs::read(&json_file).unwrap());
        remove(&ctx,&["fake".into()]).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(json_file).unwrap()).unwrap(),original);
        assert_eq!(::toml::from_str::<::toml::Value>(&std::fs::read_to_string(&toml_file).unwrap()).unwrap(),::toml::from_str::<::toml::Value>(text).unwrap());
        assert!(std::fs::read_to_string(toml_file).unwrap().contains("preserved comment"));
    }
    #[test] fn fake_setup_and_file_ownership() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
        let ctx = SetupCtx::new(&paths, tmp.path.join("home"), None).unwrap();
        let fake = crate::agents::find("fake").unwrap(); let plan = (fake.plan)(&ctx).unwrap();
        let file = ctx.home.join("fake/settings.json"); std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let original = serde_json::json!({"hooks":{"Stop":[{"command":"foreign herdr hook"}]}});
        std::fs::write(&file, original.to_string()).unwrap();
        let preview = preview(&ctx,"fake",&plan).unwrap(); assert!(preview.contains("herdr-respan"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original.to_string()); assert!(!ctx.state_dir.join("owned.json").exists());
        apply(&ctx,"fake",&plan).unwrap(); assert!(installed(&ctx.state_dir,"fake"));
        remove(&ctx,&["fake".into()]).unwrap(); assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&file).unwrap()).unwrap(),original);
        let asset = tmp.path.join("extension.js"); let plan = Plan { edits: vec![Edit::WriteFile {path:asset.clone(),contents:b"// herdr-respan original\n".to_vec(),id:"herdr-respan-asset".into()}], ..Default::default() };
        apply(&ctx,"fake",&plan).unwrap(); apply(&ctx,"fake",&plan).unwrap();
        std::fs::write(&asset,b"// external edit").unwrap(); assert!(remove(&ctx,&["fake".into()]).is_err());
        std::fs::write(&asset,b"// herdr-respan original\n").unwrap(); remove(&ctx,&["fake".into()]).unwrap(); assert!(!asset.exists());
    }
    #[test] fn foreign_additions_to_created_toml_array_survive_remove() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
        let file=tmp.path.join("config.toml");let mut doc:toml_edit::DocumentMut="[[hooks]]\ncommand='herdr-respan hook fake'\n".parse().unwrap();
        let plan=Plan {edits:vec![Edit::TomlInsert {file:file.clone(),path:vec!["hooks".into()],value:doc.remove("hooks").unwrap(),id:"herdr-respan-hook".into()}], ..Default::default()};
        apply(&ctx,"fake",&plan).unwrap();let mut current=std::fs::read_to_string(&file).unwrap();current.push_str("\n[[hooks]]\ncommand='foreign'\n");std::fs::write(&file,current).unwrap();
        remove(&ctx,&["fake".into()]).unwrap();let remaining: ::toml::Value=::toml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(remaining,::toml::from_str::<::toml::Value>("[[hooks]]\ncommand='foreign'\n").unwrap());
    }
    #[test] fn offers_default_no_interactive_yes_and_explicit_accept() {
        fn plan(file:PathBuf)->Plan {Plan {offers:vec![Offer {id:"extra",prompt:"Install extra?".into(),edits:vec![Edit::WriteFile {path:file,contents:b"herdr-respan optional".to_vec(),id:"herdr-respan-extra".into()}]}],..Default::default()}}
        for (interactive,answer,flags,expected) in [(false,false,vec![],false),(true,false,vec![],false),(true,true,vec![],true),(false,false,vec!["extra".into()],true)] {
            let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();let file=tmp.path.join("extra");let mut plan=plan(file.clone());let mut asked=false;
            accept_offers(&mut plan,&flags,interactive,|_|{asked=true;Ok(answer)}).unwrap();assert_eq!(asked,interactive&&flags.is_empty());apply(&ctx,"fake",&plan).unwrap();assert_eq!(file.exists(),expected);remove(&ctx,&["fake".into()]).unwrap();assert!(!file.exists());
        }
        let cli=crate::cli::parse("setup fake --yes --accept extra --accept other".split_whitespace().map(str::to_string)).unwrap();
        assert!(matches!(cli.command,crate::cli::Command::Setup {accept,yes:true,..} if accept==["extra","other"]));
    }
    #[test] fn text_and_textarg_offers_revert_using_original_owned_argument() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
        let files=[ctx.home.join("fake/base.txt"),ctx.home.join("fake/offer.txt"),ctx.home.join("fake/offerarg.txt")];std::fs::create_dir_all(files[0].parent().unwrap()).unwrap();for file in &files {std::fs::write(file,"foreign\n").unwrap();}
        let mut plan=(crate::agents::find("fake").unwrap().plan)(&ctx).unwrap();
        for offer in &mut plan.offers {for edit in &mut offer.edits {if let Edit::TextArg {arg,..}=edit {*arg="original".into();}}}
        accept_offers(&mut plan,&["text".into(),"textarg".into()],false,|_|Ok(false)).unwrap();apply(&ctx,"fake",&plan).unwrap();apply(&ctx,"fake",&plan).unwrap();
        let owned=Owned::load(&ctx.state_dir).unwrap();assert!(owned.entries.iter().any(|e|matches!(&e.change,Change::TextArg {arg} if arg=="original")));
        for file in &files {let mut content=std::fs::read_to_string(file).unwrap();content.push_str("later foreign\n");std::fs::write(file,content).unwrap();}
        remove(&ctx,&["fake".into()]).unwrap();for file in files {assert_eq!(std::fs::read_to_string(file).unwrap(),"foreign\nlater foreign\n");}
    }
    #[test] fn json_set_if_absent_preserves_existing_and_modified_values() {
        for original in [serde_json::json!({}),serde_json::json!({"version":1}),serde_json::json!({"version":null}),serde_json::json!({"version":2})] {
            let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();let file=tmp.path.join("settings.json");std::fs::write(&file,original.to_string()).unwrap();
            let plan=Plan {edits:vec![Edit::JsonSetIfAbsent {file:file.clone(),pointer:"/version".into(),value:serde_json::json!(1),id:"herdr-respan-version".into()}],..Default::default()};
            apply(&ctx,"fake",&plan).unwrap();apply(&ctx,"fake",&plan).unwrap();let owned=Owned::load(&ctx.state_dir).unwrap();assert_eq!(owned.entries.len(),usize::from(original.get("version").is_none()));
            remove(&ctx,&["fake".into()]).unwrap();assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&file).unwrap()).unwrap(),original);
        }
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();let file=tmp.path.join("settings.json");
        let plan=Plan {edits:vec![Edit::JsonSetIfAbsent {file:file.clone(),pointer:"/nested/enabled".into(),value:serde_json::json!(true),id:"herdr-respan-enabled".into()}],..Default::default()};apply(&ctx,"fake",&plan).unwrap();remove(&ctx,&["fake".into()]).unwrap();assert!(!file.exists());
        apply(&ctx,"fake",&plan).unwrap();std::fs::write(&file,r#"{"nested":{"enabled":false,"foreign":1}}"#).unwrap();remove(&ctx,&["fake".into()]).unwrap();assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(file).unwrap()).unwrap(),serde_json::json!({"nested":{"enabled":false,"foreign":1}}));
    }
    #[test] fn refresh_only_plans_owned_assets_and_skips_one_plan_failure() {
        fn hook(_: &mut crate::record::HookCtx,_:&[u8])->Result<()> {Ok(())}
        fn broken(_: &SetupCtx)->Result<Plan> {Err("downgraded agent".into())}
        fn unexpected(_: &SetupCtx)->Result<Plan> {panic!("config-only ownership must not call plan")}
        fn good(ctx:&SetupCtx)->Result<Plan> {Ok(Plan {edits:vec![Edit::WriteFile {path:ctx.home.join("good.js"),contents:b"new".to_vec(),id:"herdr-respan-good".into()}],..Default::default()})}
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
        for kind in ["bad","good"] {apply(&ctx,kind,&Plan {edits:vec![Edit::WriteFile {path:ctx.home.join(format!("{kind}.js")),contents:b"old".to_vec(),id:format!("herdr-respan-{kind}")}],..Default::default()}).unwrap();}
        apply(&ctx,"config-only",&Plan {edits:vec![Edit::JsonSetIfAbsent {file:ctx.home.join("settings.json"),pointer:"/version".into(),value:serde_json::json!(1),id:"herdr-respan-version".into()}],..Default::default()}).unwrap();
        let agents=[crate::agents::AgentSpec {kind:"bad",display:"bad",hook,plan:broken,reply:None,settle:None},crate::agents::AgentSpec {kind:"config-only",display:"config-only",hook,plan:unexpected,reply:None,settle:None},crate::agents::AgentSpec {kind:"good",display:"good",hook,plan:good,reply:None,settle:None}];
        refresh_owned_agents(&ctx,&agents).unwrap();assert_eq!(std::fs::read(ctx.home.join("bad.js")).unwrap(),b"old");assert_eq!(std::fs::read(ctx.home.join("good.js")).unwrap(),b"new");assert!(std::fs::read_to_string(ctx.state_dir.join("herdr-respan.log")).unwrap().contains("owned asset refresh plan failed"));
    }

    const FOREIGN_SECRET: &str = "foreign-credential-DO-NOT-JOURNAL";
    fn recovery_fixture(ctx: &SetupCtx) -> Plan {
        let mut plan = (crate::agents::find("fake").unwrap().plan)(ctx).unwrap();
        for offer in &mut plan.offers { for edit in &mut offer.edits { if let Edit::TextArg {arg, ..} = edit { *arg = "original".into(); } } }
        accept_offers(&mut plan, &["text".into(), "textarg".into()], false, |_| Ok(false)).unwrap();
        let mut tables: toml_edit::DocumentMut = "[[hooks]]\ncommand='herdr-respan first'\n[[hooks]]\ncommand='herdr-respan second'\n".parse().unwrap();
        let mut array: toml_edit::DocumentMut = "list=['herdr-respan first','herdr-respan second']".parse().unwrap();
        plan.edits.extend([
            Edit::JsonSetIfAbsent {file:ctx.home.join("fake/settings.json"), pointer:"/version".into(), value:serde_json::json!(1), id:"herdr-respan-version".into()},
            Edit::TomlInsert {file:ctx.home.join("fake/config.toml"), path:vec!["hooks".into()], value:tables.remove("hooks").unwrap(), id:"herdr-respan-tables".into()},
            Edit::TomlInsert {file:ctx.home.join("fake/config.toml"), path:vec!["list".into()], value:array.remove("list").unwrap(), id:"herdr-respan-array".into()},
            Edit::TomlInsert {file:ctx.home.join("fake/config.toml"), path:vec!["features".into(),"hooks".into()], value:toml_edit::value(true), id:"herdr-respan-flag".into()},
            Edit::WriteFile {path:ctx.home.join("fake/asset.js"), contents:b"// herdr-respan asset".to_vec(), id:"herdr-respan-asset".into()},
        ]);
        std::fs::create_dir_all(ctx.home.join("fake")).unwrap();
        std::fs::write(ctx.home.join("fake/settings.json"), serde_json::json!({"token":FOREIGN_SECRET,"hooks":{"Stop":[{"command":"foreign"}]}}).to_string()).unwrap();
        std::fs::write(ctx.home.join("fake/config.toml"), format!("# foreign comment\ntoken='{FOREIGN_SECRET}'\n[[hooks]]\ncommand='foreign'\n")).unwrap();
        for name in ["base.txt", "offer.txt", "offerarg.txt"] { std::fs::write(ctx.home.join("fake").join(name), format!("token={FOREIGN_SECRET}\n")).unwrap(); }
        plan
    }
    fn snapshots(plan: &Plan) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        plan.edits.iter().map(|e| {let path = e.file_id().0.to_path_buf(); let bytes = std::fs::read(&path).ok(); (path, bytes)}).collect()
    }
    fn restore_snapshot(path: &Path, bytes: &Option<Vec<u8>>) {
        if let Some(bytes) = bytes { std::fs::create_dir_all(path.parent().unwrap()).unwrap(); std::fs::write(path, bytes).unwrap(); }
        else if path.exists() { std::fs::remove_file(path).unwrap(); }
    }
    fn assert_restored(original: &std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for (file, before) in original {
            let after = std::fs::read(file).ok();
            match (file.extension().and_then(|s| s.to_str()), before, &after) {
                (Some("json"), Some(a), Some(b)) => assert_eq!(serde_json::from_slice::<serde_json::Value>(a).unwrap(), serde_json::from_slice::<serde_json::Value>(b).unwrap()),
                (Some("toml"), Some(a), Some(b)) => assert_eq!(::toml::from_str::<::toml::Value>(std::str::from_utf8(a).unwrap()).unwrap(), ::toml::from_str::<::toml::Value>(std::str::from_utf8(b).unwrap()).unwrap()),
                _ => assert_eq!(before, &after, "{}", file.display()),
            }
        }
    }
    fn assert_no_foreign_secret(value: &serde_json::Value) {
        assert!(!value.to_string().contains(FOREIGN_SECRET));
        match value {
            serde_json::Value::Array(items) => {
                if let Some(bytes) = items.iter().map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok())).collect::<Option<Vec<_>>>() {
                    assert!(!bytes.windows(FOREIGN_SECRET.len()).any(|b| b == FOREIGN_SECRET.as_bytes()));
                }
                for item in items { assert_no_foreign_secret(item); }
            }
            serde_json::Value::Object(items) => for item in items.values() { assert_no_foreign_secret(item); },
            _ => {},
        }
    }
    #[test] fn interrupted_setup_replays_each_write_prefix_without_journaling_foreign_secrets() {
        // Every prefix models a crash after the ownership journal, including before the first write.
        for completed in 0..=6 {
            let tmp=crate::tests::Temp::new(); let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
            let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap(); let plan=recovery_fixture(&ctx); let before=snapshots(&plan);
            apply(&ctx,"fake",&plan).unwrap(); let committed=Owned::load(&ctx.state_dir).unwrap(); let after=snapshots(&plan);
            let journal=serde_json::json!({"entries":[],"pending":{"install":committed.entries,"remove":[]}});
            assert_no_foreign_secret(&journal);
            crate::paths::atomic_json(&ctx.state_dir.join("owned.json"),&journal).unwrap();
            for (n,(file,bytes)) in before.iter().enumerate() {if n>=completed {restore_snapshot(file,bytes);}}
            assert!(!installed(&ctx.state_dir,"fake"));
            let journal_bytes=std::fs::read(ctx.state_dir.join("owned.json")).unwrap(); let interrupted=snapshots(&plan);
            preview(&ctx,"unrelated",&Plan::default()).unwrap();
            assert_eq!(snapshots(&plan),interrupted); assert_eq!(std::fs::read(ctx.state_dir.join("owned.json")).unwrap(),journal_bytes); assert!(!ctx.state_dir.join("setup.lock").exists());
            // Recovery has no supplied fake plan: callback lookup must include offers and use the saved arg.
            apply(&ctx,"unrelated",&Plan::default()).unwrap(); assert_eq!(snapshots(&plan),after);
            apply(&ctx,"fake",&plan).unwrap(); assert_eq!(snapshots(&plan),after);
            let recovered=Owned::load(&ctx.state_dir).unwrap(); assert!(recovered.pending.is_none()); assert_eq!(recovered.entries.len(),plan.edits.len());
            assert_no_foreign_secret(&serde_json::from_slice(&std::fs::read(ctx.state_dir.join("owned.json")).unwrap()).unwrap());
            remove(&ctx,&["fake".into()]).unwrap(); assert_restored(&before);
        }
    }
    #[test] fn legacy_union_retry_restores_missing_native_edits_and_original_text_arguments() {
        let tmp=crate::tests::Temp::new(); let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap(); let mut plan=recovery_fixture(&ctx); let before=snapshots(&plan);
        apply(&ctx,"fake",&plan).unwrap(); let owned=Owned::load(&ctx.state_dir).unwrap();
        // An old-style interrupted union put all planned entries in the committed array.
        crate::paths::atomic_json(&ctx.state_dir.join("owned.json"),&serde_json::json!({"entries":owned.entries})).unwrap();
        for (file,bytes) in &before {restore_snapshot(file,bytes);}
        for edit in &mut plan.edits {if let Edit::TextArg {arg,..}=edit {*arg="changed-context".into();}}
        apply(&ctx,"fake",&plan).unwrap();
        assert!(std::fs::read_to_string(ctx.home.join("fake/offerarg.txt")).unwrap().contains("arg=original"));
        assert!(!std::fs::read_to_string(ctx.home.join("fake/offerarg.txt")).unwrap().contains("changed-context"));
        assert!(ctx.home.join("fake/asset.js").exists());
        // Restore only a subset of an owned multi-member TOML insertion while retaining later foreign data.
        let file=ctx.home.join("fake/config.toml");
        std::fs::write(&file,format!("token='{FOREIGN_SECRET}'\nlist=['herdr-respan first','later foreign']\n[[hooks]]\ncommand='foreign'\n[[hooks]]\ncommand='herdr-respan first'\n[[hooks]]\ncommand='later foreign'\n")).unwrap();
        apply(&ctx,"fake",&plan).unwrap(); let repaired=std::fs::read_to_string(&file).unwrap(); assert!(repaired.contains("herdr-respan second")); assert!(repaired.contains("later foreign"));
        let once=snapshots(&plan); apply(&ctx,"fake",&plan).unwrap(); assert_eq!(snapshots(&plan),once);
        remove(&ctx,&["fake".into()]).unwrap(); let removed=std::fs::read_to_string(file).unwrap(); assert!(!removed.contains("herdr-respan")); assert!(removed.contains("later foreign")); assert!(removed.contains(FOREIGN_SECRET));
    }
    #[test] fn interrupted_remove_replays_each_write_prefix() {
        for completed in 0..=6 {
            let tmp=crate::tests::Temp::new(); let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
            let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap(); let plan=recovery_fixture(&ctx); let original=snapshots(&plan);
            apply(&ctx,"fake",&plan).unwrap(); let owned=Owned::load(&ctx.state_dir).unwrap(); let installed_files=snapshots(&plan);
            remove(&ctx,&["fake".into()]).unwrap(); let removed=snapshots(&plan);
            let removals:Vec<_>=owned.entries.iter().rev().cloned().collect();
            crate::paths::atomic_json(&ctx.state_dir.join("owned.json"),&serde_json::json!({"entries":owned.entries,"pending":{"install":[],"remove":removals}})).unwrap();
            for (n,(file,bytes)) in installed_files.iter().enumerate() {if n>=completed {restore_snapshot(file,bytes);} else {restore_snapshot(file,&removed[file]);}}
            remove(&ctx,&["unrelated".into()]).unwrap(); assert_restored(&original);
            let recovered=Owned::load(&ctx.state_dir).unwrap(); assert!(recovered.entries.is_empty()); assert!(recovered.pending.is_none());
            remove(&ctx,&["fake".into()]).unwrap(); assert_restored(&original);
        }
    }
    #[test] fn recovery_refuses_unidentifiable_text_without_claiming_installation() {
        let tmp=crate::tests::Temp::new(); let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap(); let file=tmp.path.join("unknown.txt"); std::fs::write(&file,"foreign").unwrap();
        let entry=Entry {agent:"missing-adapter".into(),id:"herdr-respan-unknown".into(),file:file.clone(),existed:true,change:Change::Text};
        crate::paths::atomic_json(&ctx.state_dir.join("owned.json"),&serde_json::json!({"entries":[],"pending":{"install":[entry],"remove":[]}})).unwrap();
        let before=std::fs::read(ctx.state_dir.join("owned.json")).unwrap();
        assert!(apply(&ctx,"unrelated",&Plan::default()).is_err()); assert!(remove(&ctx,&[]).is_err());
        assert_eq!(std::fs::read(ctx.state_dir.join("owned.json")).unwrap(),before); assert_eq!(std::fs::read_to_string(file).unwrap(),"foreign"); assert!(!installed(&ctx.state_dir,"missing-adapter"));
    }
    #[test] fn concurrent_disjoint_setups_keep_both_ownership_sets() {
        use std::{cell::RefCell, sync::mpsc};
        thread_local! { static HOLD:RefCell<Option<(mpsc::Sender<()>,mpsc::Receiver<()>)>>=const {RefCell::new(None)}; }
        fn held_apply(before:&str)->Result<String> {
            HOLD.with(|hold| {
                let guard=hold.borrow(); let (entered,release)=guard.as_ref().unwrap(); entered.send(())?; release.recv()?; Ok(format!("{before}herdr-respan held\n"))
            })
        }
        fn revert(before:&str)->Result<String> {Ok(before.replace("herdr-respan held\n",""))}
        let tmp=crate::tests::Temp::new(); let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
        let first=Plan {edits:vec![Edit::Text {file:tmp.path.join("first.txt"),id:"herdr-respan-first".into(),apply:held_apply,revert}],..Default::default()};
        let second=Plan {edits:vec![Edit::WriteFile {path:tmp.path.join("second.js"),contents:b"herdr-respan second".to_vec(),id:"herdr-respan-second".into()}],..Default::default()};
        let (entered_tx,entered_rx)=mpsc::channel(); let (release_tx,release_rx)=mpsc::channel();
        let second_was_blocked=std::thread::scope(|scope| {
            let ctx_ref=&ctx; let first_ref=&first;
            let a=scope.spawn(move || {
                HOLD.with(|hold| *hold.borrow_mut()=Some((entered_tx,release_rx)));
                apply(ctx_ref,"first",first_ref)
            }); entered_rx.recv().unwrap();
            let (done_tx,done_rx)=mpsc::channel();
            let ctx_ref=&ctx; let second_ref=&second;
            let b=scope.spawn(move || {let result=apply(ctx_ref,"second",second_ref);done_tx.send(()).unwrap();result});
            let blocked=matches!(done_rx.recv_timeout(std::time::Duration::from_millis(150)),Err(mpsc::RecvTimeoutError::Timeout));
            release_tx.send(()).unwrap(); a.join().unwrap().unwrap(); b.join().unwrap().unwrap(); blocked
        });
        assert!(second_was_blocked,"second transaction entered while first was staging");
        let owned=Owned::load(&ctx.state_dir).unwrap(); assert_eq!(owned.entries.len(),2); assert!(installed(&ctx.state_dir,"first")); assert!(installed(&ctx.state_dir,"second"));
        assert!(tmp.path.join("first.txt").exists()); assert!(tmp.path.join("second.js").exists());
    }
    #[test] fn interrupted_asset_refresh_restores_missing_assets_including_owned_offers() {
        fn hook(_: &mut crate::record::HookCtx,_:&[u8])->Result<()> {Ok(())}
        fn assets(ctx:&SetupCtx)->Result<Plan> {Ok(Plan {edits:vec![Edit::WriteFile {path:ctx.home.join("a.js"),contents:b"new-a".to_vec(),id:"herdr-respan-a".into()}],offers:vec![Offer {id:"asset",prompt:"asset".into(),edits:vec![Edit::WriteFile {path:ctx.home.join("b.js"),contents:b"new-b".to_vec(),id:"herdr-respan-b".into()}]}],..Default::default()})}
        let tmp=crate::tests::Temp::new(); let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
        let mut plan=assets(&ctx).unwrap(); accept_offers(&mut plan,&["asset".into()],false,|_|Ok(false)).unwrap();
        for edit in &mut plan.edits {if let Edit::WriteFile {contents,..}=edit {*contents=b"old".to_vec();}}
        apply(&ctx,"assets",&plan).unwrap(); let old=Owned::load(&ctx.state_dir).unwrap();
        let mut new_plan=assets(&ctx).unwrap(); accept_offers(&mut new_plan,&["asset".into()],false,|_|Ok(false)).unwrap(); apply(&ctx,"assets",&new_plan).unwrap(); let new=Owned::load(&ctx.state_dir).unwrap();
        // First refresh write completed, second asset disappeared before retry.
        std::fs::remove_file(ctx.home.join("b.js")).unwrap();
        crate::paths::atomic_json(&ctx.state_dir.join("owned.json"),&serde_json::json!({"entries":old.entries,"pending":{"install":new.entries,"remove":[]}})).unwrap();
        let spec=crate::agents::AgentSpec {kind:"assets",display:"assets",hook,plan:assets,reply:None,settle:None};
        refresh_owned_agents(&ctx,std::slice::from_ref(&spec)).unwrap();
        assert_eq!(std::fs::read(ctx.home.join("a.js")).unwrap(),b"new-a"); assert_eq!(std::fs::read(ctx.home.join("b.js")).unwrap(),b"new-b");
        let owned=Owned::load(&ctx.state_dir).unwrap(); assert_eq!(owned.entries.len(),2); assert!(owned.pending.is_none());
        std::fs::remove_file(ctx.home.join("b.js")).unwrap();
        refresh_owned_agents(&ctx,std::slice::from_ref(&spec)).unwrap();
        assert_eq!(std::fs::read(ctx.home.join("b.js")).unwrap(),b"new-b");
    }

    #[test] fn setup_journal_contains_only_owned_operations_and_committed_ownership() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();let plan=recovery_fixture(&ctx);
        let (files,owned)=stage(&ctx,"fake",&plan,files::Files::new(),Owned::default()).unwrap();
        assert!(files.values().any(|f| f.before.as_ref().is_some_and(|b| String::from_utf8_lossy(b).contains(FOREIGN_SECRET))));
        files::journal(&owned,&ctx.state_dir).unwrap();
        let journal:serde_json::Value=serde_json::from_slice(&std::fs::read(ctx.state_dir.join("owned.json")).unwrap()).unwrap();
        assert_no_foreign_secret(&journal);assert!(journal["entries"].as_array().unwrap().is_empty());
        assert_eq!(journal["pending"]["install"].as_array().unwrap().len(),plan.edits.len());
        assert!(!installed(&ctx.state_dir,"fake"));assert!(!ctx.home.join("fake/asset.js").exists());
        apply(&ctx,"unrelated",&Plan::default()).unwrap();assert!(installed(&ctx.state_dir,"fake"));
    }

    #[test] fn legacy_asset_union_selects_installed_version_and_rejects_ambiguous_bytes() {
        for current in [Some(b"old".as_slice()),Some(b"new".as_slice()),None,Some(b"foreign".as_slice())] {
            let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
            let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();let file=tmp.path.join("asset.js");
            let mut plan=Plan {edits:vec![Edit::WriteFile {path:file.clone(),contents:b"old".to_vec(),id:"herdr-respan-asset".into()}],..Default::default()};
            apply(&ctx,"assets",&plan).unwrap();let mut union=Owned::load(&ctx.state_dir).unwrap().entries;
            if let Edit::WriteFile {contents,..}=&mut plan.edits[0] {*contents=b"new".to_vec();}
            apply(&ctx,"assets",&plan).unwrap();union.extend(Owned::load(&ctx.state_dir).unwrap().entries);
            crate::paths::atomic_json(&ctx.state_dir.join("owned.json"),&serde_json::json!({"entries":union})).unwrap();
            restore_snapshot(&file,&current.map(Vec::from));let before=std::fs::read(ctx.state_dir.join("owned.json")).unwrap();
            if current==Some(b"old".as_slice())||current==Some(b"new".as_slice()) {
                apply(&ctx,"assets",&plan).unwrap();assert_eq!(std::fs::read(&file).unwrap(),b"new");assert_eq!(Owned::load(&ctx.state_dir).unwrap().entries.len(),1);
                remove(&ctx,&["assets".into()]).unwrap();assert!(!file.exists());
            } else {
                assert!(apply(&ctx,"assets",&plan).is_err());assert_eq!(std::fs::read(&file).ok(),current.map(Vec::from));assert_eq!(std::fs::read(ctx.state_dir.join("owned.json")).unwrap(),before);
            }
        }
    }

    #[test] fn binary_refresh_serializes_with_setup_and_preserves_foreign_temp_file() {
        use std::sync::mpsc;
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let binary=paths.binary();std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        let foreign_tmp=binary.with_extension("new");std::fs::write(&foreign_tmp,b"foreign temporary file").unwrap();
        let lock=transaction_lock(&paths.state).unwrap();
        let blocked=std::thread::scope(|scope| {
            let (done_tx,done_rx)=mpsc::channel();let paths_ref=&paths;
            let first=scope.spawn(move || {let result=refresh_binary(paths_ref);done_tx.send(()).unwrap();result});
            let blocked=matches!(done_rx.recv_timeout(std::time::Duration::from_millis(150)),Err(mpsc::RecvTimeoutError::Timeout));
            drop(lock);
            let second=scope.spawn(||refresh_binary(&paths));
            first.join().unwrap().unwrap();second.join().unwrap().unwrap();blocked
        });
        assert!(blocked,"binary refresh entered during the setup transaction");
        assert_eq!(std::fs::read(&binary).unwrap(),std::fs::read(std::env::current_exe().unwrap()).unwrap());
        assert_eq!(std::fs::read_to_string(paths.state.join("bin/version")).unwrap(),env!("CARGO_PKG_VERSION"));
        assert_eq!(std::fs::read(foreign_tmp).unwrap(),b"foreign temporary file");
        assert!(!paths.state.join("owned.json").exists());assert!(!paths.state.join("setup.lock").exists());
    }
}
