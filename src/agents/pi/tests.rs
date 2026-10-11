use super::{setup, SPEC};
use crate::{agents::common::bridge::Batch, config::Config, paths::Paths, record::{HookCtx, SpanRecord}, setup::{Edit, SetupCtx}};
use std::{collections::BTreeMap, path::Path};

const GOLDEN: &[u8] = include_bytes!("fixtures/batch.json");

fn context(tmp: &crate::tests::Temp, proxy: bool) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") },
        Config { customer_identifier: "synthetic-host".into(), ..Default::default() }, "pi",
        BTreeMap::new(), None, proxy).unwrap()
}
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|path| {
        std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
    }).collect()
}

#[test]
fn golden_batch_shape_and_spool_tree() {
    let batch: Batch = serde_json::from_slice(GOLDEN).unwrap();
    assert_eq!(batch.agent, "pi");
    assert_eq!(batch.events.len(), 6);
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp, false);
    (SPEC.hook)(&mut ctx, GOLDEN).unwrap();
    ctx.finish().unwrap();
    let spans = records(&tmp);
    assert_eq!(spans.len(), 5);
    let turn = spans.iter().find(|span| span.log_type == "agent").unwrap();
    assert_eq!(turn.span_key, "pi:synthetic-session:t1:turn");
    assert_eq!(turn.start_ns, 1_000_000_000);
    assert_eq!(turn.end_ns, 1_140_000_000);
    assert_eq!(turn.thread_id.as_deref(), Some("synthetic-session"));
    for child in spans.iter().filter(|span| span.log_type != "agent") {
        assert_eq!(child.parent_key.as_deref(), Some(turn.span_key.as_str()));
    }
    let chat = spans.iter().find(|span| span.span_key.ends_with("response-2")).unwrap();
    assert_eq!(chat.usage.input_tokens, Some(10));
    assert_eq!(chat.usage.cache_creation_input_tokens, Some(0));
    assert_eq!(chat.usage.reasoning_tokens, Some(1));
    assert_eq!(chat.model.as_deref(), Some("haiku-resolved"));
    assert_eq!(chat.metadata.get("estimated_cost_usd"), Some(&serde_json::json!(0.0000332)));
    let first = spans.iter().find(|span| span.span_key.ends_with("response-1")).unwrap();
    assert_eq!(first.usage.reasoning_tokens, None);
    assert!(spans.iter().any(|span| span.name == "compaction" && span.log_type == "tool"));
    (SPEC.hook)(&mut ctx, GOLDEN).unwrap();
    ctx.finish().unwrap();
    assert_eq!(records(&tmp).len(), 5);
}

#[test]
fn foreign_payloads_rejected_and_proxy_suppression_shared() {
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp, true);
    let mut foreign: serde_json::Value = serde_json::from_slice(GOLDEN).unwrap();
    foreign["agent"] = serde_json::json!("omp");
    assert!((SPEC.hook)(&mut ctx, &serde_json::to_vec(&foreign).unwrap()).is_err());
    assert!((SPEC.hook)(&mut ctx, br#"{"session_id":"s","hook_event_name":"Stop"}"#).is_err());
    assert!((SPEC.hook)(&mut ctx, b"not-json").is_err());
    assert!(records(&tmp).is_empty());
    (SPEC.hook)(&mut ctx, GOLDEN).unwrap();
    ctx.finish().unwrap();
    let spans = records(&tmp);
    assert_eq!(spans.len(), 3);
    assert!(spans.iter().all(|span| span.log_type != "chat"));
}

#[test]
fn setup_asset_preserves_herdr_and_remove_roundtrips() {
    let tmp = crate::tests::Temp::new();
    let paths = Paths { state: tmp.path.join("state with spaces"), config: tmp.path.join("config") };
    let ctx = SetupCtx::new(&paths, tmp.path.join("home"), None).unwrap();
    let plan = setup::plan_for(&ctx, None).unwrap();
    let extension = ctx.home.join(".pi/agent/extensions/herdr-respan.ts");
    let herdr = extension.with_file_name("herdr-agent-state.ts");
    std::fs::create_dir_all(herdr.parent().unwrap()).unwrap();
    std::fs::write(&herdr, b"// existing herdr-managed integration\n").unwrap();
    let settings = ctx.home.join(".pi/agent/settings.json");
    let original = br#"{"theme":"dark","extensions":["foreign-extension"]}"#;
    std::fs::write(&settings, original).unwrap();
    assert!(crate::setup::preview(&ctx, "pi", &plan).unwrap().contains("herdr-respan.ts"));
    assert!(!extension.exists());
    crate::setup::apply(&ctx, "pi", &plan).unwrap();
    let installed = std::fs::read_to_string(&extension).unwrap();
    assert!(!installed.contains("__HERDR_RESPAN_"));
    assert!(installed.contains(&serde_json::to_string(&ctx.binary.to_string_lossy()).unwrap()));
    assert!(installed.contains("\"hook\", \"pi\", \"--state-dir\""));
    crate::setup::apply(&ctx, "pi", &plan).unwrap();
    assert_eq!(std::fs::read_to_string(&extension).unwrap(), installed);
    crate::setup::remove(&ctx, &["pi".into()]).unwrap();
    assert!(!extension.exists());
    assert_eq!(std::fs::read(&herdr).unwrap(), b"// existing herdr-managed integration\n");
    assert_eq!(std::fs::read(&settings).unwrap(), original);
}

#[test]
fn setup_honors_relocation_tilde_and_project_scope() {
    let tmp = crate::tests::Temp::new();
    let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
    let mut ctx = SetupCtx::new(&paths, tmp.path.join("home"), None).unwrap();
    let relocated = tmp.path.join("relocated");
    for (input, expected) in [(relocated.as_path(), relocated.join("extensions/herdr-respan.ts")),
        (Path::new("~/custom-pi"), ctx.home.join("custom-pi/extensions/herdr-respan.ts"))] {
        let plan = setup::plan_for(&ctx, Some(input)).unwrap();
        assert!(matches!(&plan.edits[..], [Edit::WriteFile { path, .. }] if path == &expected));
    }
    ctx.project = Some(tmp.path.join("project"));
    let plan = setup::plan_for(&ctx, Some(&relocated)).unwrap();
    assert!(matches!(&plan.edits[..], [Edit::WriteFile { path, .. }] if path == &tmp.path.join("project/.pi/extensions/herdr-respan.ts")));
}
