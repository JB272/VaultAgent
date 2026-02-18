use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use vaultagent_core::{load_config, run, RunRequest};

#[derive(Parser)]
#[command(name = "vaultagent", version, about = "VaultAgent CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        task: String,

        #[arg(long, default_value = "configs/default.toml")]
        config: PathBuf,

        #[arg(long = "enable-tool")]
        enable_tool: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run {
            task,
            config,
            enable_tool,
        } => {
            let cfg = load_config(&config).with_context(|| "failed loading config")?;
            let result = run(
                RunRequest {
                    task,
                    enabled_tools: enable_tool,
                },
                &cfg,
            )?;

            println!("run_id: {}", result.run_id);
            println!("audit_dir: {}", result.run_dir.display());
            println!("result:\n{}", result.final_text);
        }
    }

    Ok(())
}
