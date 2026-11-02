//! The `jefe` command.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "jefe",
    version,
    about = "Delegate a goal to coding agents and supervise them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// What this build of jefe supports.
    Capabilities {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Capabilities { json } => {
            let caps = jefe::capabilities();
            if json {
                println!("{}", serde_json::to_string(&caps)?);
            } else {
                println!("jefe {} (protocol {}, acc {})", caps.jefe, caps.version, caps.acc);
                println!("harnesses: {}", caps.harnesses.join(", "));
                println!("brains: {}", caps.brains.join(", "));
            }
        }
    }
    Ok(())
}
