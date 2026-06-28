//! Context-assembler behavior, exercised against the concrete blackboard.
//!
//! The assembler lives in `rinne-loop` (over the `Blackboard` trait), but the
//! concrete file/SQLite blackboard lives here in `rinne-core` — so these
//! end-to-end checks of per-family context shaping live with the concrete impl.

use std::path::PathBuf;

use rinne_core::worker::WorkerFamily;
use rinne_core::{Blackboard, Plan};
use rinne_loop::assembler::ContextAssembler;

fn temp_workspace(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rinne-asm-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn plan_with_mention() -> Plan {
    serde_json::from_value(serde_json::json!({
        "goal": "g",
        "mentioned": ["src/a.rs"],
        "nodes": [{
            "id": "n1",
            "role": "generator",
            "instruction": "do it",
            "needs": ["code-edit"],
            "inputs": ["design.md"]
        }]
    }))
    .unwrap()
}

#[test]
fn harness_pins_paths_inlines_nothing() {
    let ws = temp_workspace("harness");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/a.rs"), "fn a() {}").unwrap();
    let bb = Blackboard::open(&ws).unwrap();
    bb.write_artifact("design.md", "the design").unwrap();
    let plan = plan_with_mention();

    let asm = ContextAssembler::new(&bb, &plan);
    let packet = asm.build(&plan.nodes[0], WorkerFamily::Harness, None).unwrap();

    assert!(packet.inlined_files.is_empty());
    assert!(packet.pinned_paths.contains(&PathBuf::from("src/a.rs")));
    assert!(packet
        .pinned_paths
        .iter()
        .any(|p| p.ends_with("artifacts/design.md")));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn api_inlines_contents_pins_nothing() {
    let ws = temp_workspace("api");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/a.rs"), "fn a() {}").unwrap();
    let bb = Blackboard::open(&ws).unwrap();
    bb.write_artifact("design.md", "the design").unwrap();
    let plan = plan_with_mention();

    let asm = ContextAssembler::new(&bb, &plan);
    let packet = asm.build(&plan.nodes[0], WorkerFamily::Api, None).unwrap();

    assert!(packet.pinned_paths.is_empty());
    let contents: Vec<&str> = packet.inlined_files.iter().map(|f| f.contents.as_str()).collect();
    assert!(contents.contains(&"fn a() {}"));
    assert!(contents.contains(&"the design"));

    let _ = std::fs::remove_dir_all(&ws);
}
