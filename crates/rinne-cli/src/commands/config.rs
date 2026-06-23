//! `rinne config` — show the resolved configuration and where it comes from
//! (`CONTEXT.md` §17, §18).
//!
//! Phase 1 surfaces the effective config and the file locations the user edits.
//! Rinne never writes secrets here; keys live in env vars per §9.

use anyhow::Result;

use rinne_config::paths;

pub async fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config = rinne_config::load_cwd()?;

    println!("rinne config — resolved (defaults ← global ← project ← env)\n");
    println!("{}", toml::to_string_pretty(&config)?);

    println!("Sources (later overrides earlier):");
    match paths::global_config_file() {
        Some(p) => println!("  global   {}  {}", existence(&p), p.display()),
        None => println!("  global   (no home directory found)"),
    }
    let project = paths::project_config_file(&cwd);
    println!("  project  {}  {}", existence(&project), project.display());
    println!("  env      RINNE_* environment variables");
    Ok(())
}

fn existence(p: &std::path::Path) -> &'static str {
    if p.exists() {
        "[present]"
    } else {
        "[absent] "
    }
}
