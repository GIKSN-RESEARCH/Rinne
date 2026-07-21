//! `rinne doctor` — detect and report backends, auth mode, and quota
//! (`CONTEXT.md` §9, §17).

use anyhow::Result;

use rinne_conductor::{format_routing_report, load_user_exemplars, resolve_openai};
use rinne_config::{
    probe::{WorkerFamily, WorkerStatus},
    Config, DoctorReport,
};

/// Run the probe and print a human-readable report.
pub async fn run(refresh: bool, routing: bool) -> Result<()> {
    let config = rinne_config::load_cwd()?;
    if routing {
        print_routing_report(&config).await?;
        return Ok(());
    }
    let report = rinne_config::doctor(&config, refresh).await?;
    print_report(&config, &report);
    Ok(())
}

async fn print_routing_report(config: &Config) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (registry, _) = crate::runner::build_registry(config).await?;
    let exemplars_path = config
        .routing
        .exemplars_file
        .as_ref()
        .map(|p| cwd.join(p))
        .unwrap_or_else(|| cwd.join(".rinne/routing-exemplars.toml"));
    let user_exemplars = load_user_exemplars(&exemplars_path);
    let api_key_present = resolve_openai(&config.conductor)?.is_some();
    let report = format_routing_report(
        &config.conductor,
        &config.routing,
        &registry.descriptors(),
        &user_exemplars,
        api_key_present,
    );
    print!("{report}");
    Ok(())
}

fn print_report(config: &Config, report: &DoctorReport) {
    println!("rinne doctor — worker status\n");

    print_family("HARNESS WORKERS", report, WorkerFamily::Harness);
    println!();
    print_family("API WORKERS", report, WorkerFamily::Api);

    let conductor = &config.conductor;
    println!(
        "\nCONDUCTOR\n  backend {} · model {}",
        format_backend(conductor),
        conductor.model
    );

    if !report.warnings.is_empty() {
        println!("\n⚠ WARNINGS");
        for w in &report.warnings {
            println!("  • {w}");
        }
    }

    if !report.recommendations.is_empty() {
        println!("\n💡 RECOMMENDATIONS");
        for r in &report.recommendations {
            println!("  • {r}");
        }
    }

    let available = report.available().count();
    println!(
        "\n{available} worker(s) available. {} metered.",
        report
            .available()
            .filter(|w| w.auth_mode.is_metered())
            .count()
    );
}

fn print_family(title: &str, report: &DoctorReport, family: WorkerFamily) {
    println!("{title}");
    let mut any = false;
    for w in report.workers.iter().filter(|w| w.family == family) {
        any = true;
        let (mark, state) = match &w.status {
            WorkerStatus::Available => ("✔", "available".to_string()),
            WorkerStatus::NotInstalled => ("·", "not installed".to_string()),
            WorkerStatus::SmokeTestFailed(why) => ("✗", format!("error: {why}")),
        };
        // Only flag the actionable case: installed and usable, but not in the
        // enabled list, so Rinne won't route to it until the user opts in.
        let note = if w.status.is_available() && !w.enabled {
            "  ← installed; add to [backends.harness] enabled to use"
        } else {
            ""
        };
        println!(
            "  {mark} {:<14} {:<14} {}{}",
            w.name,
            w.auth_mode.label(),
            state,
            note
        );
        for warn in &w.warnings {
            println!("      ⚠ {warn}");
        }
    }
    if !any {
        println!("  (none configured)");
    }
}

fn format_backend(c: &rinne_config::model::ConductorConfig) -> String {
    format!("{:?}", c.backend).to_lowercase()
}
