//! The canonical capability catalog the conductor plans over (`MCP_SKILLS.md`
//! §11). Gathers the cheap name+description layer for every configured MCP tool
//! and installed skill, so the planner can attach them to nodes by id/name. The
//! full tool schema / skill body loads later, only when an attached node runs.

use std::path::Path;

use rinne_conductor::{SkillInfo, ToolInfo};
use rinne_config::model::Config;

/// The combined tool + skill catalog passed into a [`ConductorInput`].
///
/// [`ConductorInput`]: rinne_conductor::ConductorInput
#[derive(Debug, Default)]
pub struct Catalog {
    pub tools: Vec<ToolInfo>,
    pub skills: Vec<SkillInfo>,
}

/// Build the catalog from config + the working repo. Skills are read from disk
/// (cheap); tools require a live connection per enabled server, so this is
/// best-effort — an unreachable server is logged and skipped, never fatal to
/// planning.
pub async fn gather(config: &Config, cwd: &Path) -> Catalog {
    let skills = rinne_config::skills::discover(cwd)
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
        })
        .collect();

    let tools = crate::mcp_pool::McpPool::from_config(config)
        .list_all_tools()
        .await
        .into_iter()
        .map(|(id, t)| ToolInfo {
            id,
            description: t.description,
        })
        .collect();

    Catalog { tools, skills }
}
