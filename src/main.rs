mod agent_setup;
mod claude;
mod cli;
mod cmd;
mod command;
mod config;
mod git;
mod github;
mod llm;
mod logger;
mod markdown;
mod multiplexer;
mod naming;
mod nerdfont;
mod prompt;
mod sandbox;
mod shell;
mod skills;
mod spinner;
mod state;
mod target;
mod template;
mod util;
mod workflow;

use anyhow::Result;
use tracing::{error, info};

fn main() -> Result<()> {
    logger::init()?;

    // Build a root span with nsmux correlation context.
    // These env vars are set by nsmux for every terminal surface.
    let surface_id = std::env::var("CMUX_SURFACE_ID").unwrap_or_default();
    let workspace_id = std::env::var("CMUX_WORKSPACE_ID").unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let _root = tracing::info_span!(
        "workmux",
        surface = %surface_id,
        workspace = %workspace_id,
        cwd = %cwd,
    )
    .entered();

    info!(args = ?std::env::args().collect::<Vec<_>>(), "start");

    match cli::run() {
        Ok(result) => {
            info!("finished");
            Ok(result)
        }
        Err(err) => {
            error!(error = ?err, "failed");
            Err(err)
        }
    }
}
