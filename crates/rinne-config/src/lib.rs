//! `rinne-config` — figment + serde configuration and the `doctor` probe
//! (`CONTEXT.md` §9, §18; `PHASE.md` P1).
//!
//! Loads the layered config (defaults ← global ← per-project ← env) and probes
//! the machine for installed workers, classifying each one's auth mode and
//! catching the Claude `ANTHROPIC_API_KEY` billing footgun.

pub mod cache;
pub mod known;
pub mod load;
pub mod model;
pub mod paths;
pub mod probe;
pub mod secrets;
pub mod skills;
pub mod update;
pub mod write;

pub use load::{load, load_cwd};
pub use model::Config;
pub use probe::{AuthMode, DoctorReport, WorkerFamily, WorkerProbe, WorkerStatus};

use rinne_core::Result;

/// Run `doctor`. Reuses cached installation detection when fresh (unless
/// `refresh` forces a re-probe), but always classifies auth mode and the
/// footgun from the live environment. Fresh installation detection is written
/// back to the cache.
pub async fn doctor(config: &Config, refresh: bool) -> Result<DoctorReport> {
    let cached = if refresh { None } else { cache::load_fresh() };
    let outcome = probe::run(config, cached.as_ref()).await?;
    // Caching is best-effort: a failure to write must not fail `doctor`.
    if let Err(e) = cache::store(&outcome.installs) {
        tracing::warn!("could not cache doctor probe: {e}");
    }
    Ok(outcome.report)
}
