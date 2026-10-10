pub mod json;
pub mod toml;
mod files;
mod owned;
mod diff;
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
    TomlInsert { file: PathBuf, path: Vec<String>, value: toml_edit::Item, id: String },
    WriteFile { path: PathBuf, contents: Vec<u8>, id: String },
    Text { file: PathBuf, id: String, apply: fn(&str) -> Result<String>, revert: fn(&str) -> Result<String> },
}
impl Edit {
    fn file_id(&self) -> (&Path, &str) { match self { Self::JsonInsert { file,id,.. } | Self::TomlInsert { file,id,.. } | Self::Text { file,id,.. } => (file,id), Self::WriteFile { path,id,.. } => (path,id) } }
}
fn stage(ctx: &SetupCtx, agent: &str, plan: &Plan) -> Result<(files::Files, Owned)> {
    let mut owned = Owned::load(&ctx.state_dir)?; let mut files = files::Files::new();
    for edit in &plan.edits {
        let (file, id) = edit.file_id();
        if !file.is_absolute() || !id.contains("herdr-respan") { return Err("setup requires absolute paths and herdr-respan ownership markers".into()); }
        if file.file_name().is_some_and(|s| s == "auth.json" || s == "credentials.json" || s == ".env") || file.starts_with(ctx.home.join(".respan")) { return Err("setup may not edit credential stores".into()); }
        let previous = owned.entries.iter().position(|e| e.agent == agent && e.id == id && e.file == file);
        let staged = files::get(&mut files, file)?; let existed = staged.before.is_some() && !owned.entries.iter().any(|e| e.file == file && !e.existed);
        if let Some(i) = previous {
            if let (Edit::WriteFile { contents, .. }, Change::File { contents: old }) = (edit, &owned.entries[i].change) {
                if staged.after.as_deref() != Some(old.as_slice()) { return Err("owned asset was edited externally; refusing overwrite".into()); }
                staged.after = Some(contents.clone()); owned.entries[i].change = Change::File { contents: contents.clone() };
            }
            continue;
        }
        let change = match edit {
            Edit::JsonInsert { pointer, value, .. } => {
                if !value.to_string().contains("herdr-respan") { return Err("JSON entry lacks ownership marker".into()); }
                let mut root = match &staged.after { Some(b) => serde_json::from_slice(b)?, None => serde_json::json!({}) };
                let (added, mut created) = json::insert(&mut root, pointer, value)?;
                if !added { continue; }
                for entry in &owned.entries { if entry.file == file { if let Change::Json { created: inherited, .. } = &entry.change { created.extend(inherited.iter().cloned()); } } }
                staged.after = Some(serde_json::to_vec_pretty(&root)?);
                Change::Json { pointer: pointer.clone(), value: value.clone(), created }
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
        };
        owned.entries.push(Entry { agent: agent.into(), id: id.into(), file: file.into(), existed, change });
    }
    Ok((files, owned))
}
pub fn accept_offers(plan: &mut Plan, accepted: &[String], interactive: bool, mut ask: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    for offer in &mut plan.offers {
        if accepted.iter().any(|id| id == offer.id) || (interactive && ask(&offer.prompt)?) { plan.edits.append(&mut offer.edits); }
    }
    Ok(())
}
pub fn preview(ctx: &SetupCtx, agent: &str, plan: &Plan) -> Result<String> { let (files, _) = stage(ctx, agent, plan)?; Ok(diff::render(&files)) }
/// Caller must obtain explicit user confirmation before applying the previewed plan.
pub fn apply(ctx: &SetupCtx, agent: &str, plan: &Plan) -> Result<()> { let (files, owned) = stage(ctx, agent, plan)?; files::commit(&files, &owned, &ctx.state_dir) }
pub fn remove(ctx: &SetupCtx, agents: &[String]) -> Result<()> {
    let mut owned = Owned::load(&ctx.state_dir)?; let mut files = files::Files::new();
    for entry in owned.entries.iter().rev().filter(|e| agents.is_empty() || agents.contains(&e.agent)) {
        let staged = files::get(&mut files, &entry.file)?;
        let Some(bytes) = &staged.after else { continue; };
        match &entry.change {
            Change::Json { pointer, value, created } => {
                let mut root: serde_json::Value = serde_json::from_slice(bytes)?;
                json::remove(&mut root, pointer, value, created)?;
                staged.after = if !entry.existed && root.as_object().is_some_and(|o| o.is_empty()) { None } else { Some(serde_json::to_vec_pretty(&root)?) };
            }
            Change::Toml { path, value, appended, created } => {
                let mut doc: toml_edit::DocumentMut = std::str::from_utf8(bytes)?.parse()?; toml::remove(&mut doc, path, &toml::decode(value)?, *appended, created)?;
                staged.after = if !entry.existed && doc.as_table().is_empty() { None } else { Some(doc.to_string().into_bytes()) };
            }
            Change::File { contents } => { if bytes == contents { staged.after = None; } else { return Err("owned asset modified; refusing removal".into()); } }
            Change::Text => {
                let current = std::str::from_utf8(bytes)?;
                let spec = crate::agents::find(&entry.agent).ok_or("text adapter no longer available")?;
                let plan = (spec.plan)(ctx)?;
                let revert = plan.edits.iter().find_map(|e| match e { Edit::Text { id, revert, .. } if id == &entry.id => Some(revert), _ => None }).ok_or("missing text revert")?;
                let reverted = revert(current)?;
                staged.after = if !entry.existed && reverted.is_empty() { None } else { Some(reverted.into_bytes()) };
            }
        }
    }
    owned.entries.retain(|e| !agents.is_empty() && !agents.contains(&e.agent));
    files::commit(&files, &owned, &ctx.state_dir)
}
pub fn installed(state: &Path, agent: &str) -> bool { Owned::load(state).is_ok_and(|o| o.entries.iter().any(|e| e.agent == agent)) }
pub fn refresh_binary(paths: &Paths) -> Result<()> {
    let current = std::env::current_exe()?; let binary = paths.binary();
    if current == binary { return Ok(()); }
    let version = paths.state.join("bin/version");
    if binary.exists() && std::fs::read_to_string(&version).ok().as_deref() == Some(env!("CARGO_PKG_VERSION")) { return Ok(()); }
    std::fs::create_dir_all(binary.parent().ok_or("missing binary parent")?)?;
    let tmp = binary.with_extension("new"); std::fs::copy(current, &tmp)?; std::fs::rename(tmp, binary)?;
    crate::paths::write_private(&version, env!("CARGO_PKG_VERSION").as_bytes())
}
pub fn refresh_owned(ctx: &SetupCtx) -> Result<()> {
    let owned = Owned::load(&ctx.state_dir)?;
    for agent in crate::agents::AGENTS {
        if owned.entries.iter().any(|e| e.agent == agent.kind) {
            let mut plan = (agent.plan)(ctx)?;
            plan.edits.retain(|e| matches!(e, Edit::WriteFile { path, id, .. } if owned.entries.iter().any(|o| o.agent == agent.kind && &o.file == path && &o.id == id)));
            apply(ctx, agent.kind, &plan)?;
        }
    }
    Ok(())
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
}
