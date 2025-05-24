mod index;
mod install;
mod manifest;
mod r#ref;
mod sandbox;

use std::sync::Arc;

use crate::{index::get_index, r#ref::Ref, sandbox::run_sandboxed};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use composefs::fsverity::Sha256HashValue;

#[derive(Parser)]
#[command(
    name = "flatpak-next",
    version,
    about = "flatpak-next demo on composefs-rs"
)]
struct Args {
    #[clap(long, default_value = "https://registry.fedoraproject.org/")]
    repository: String,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    List,
    Search {
        term: String,
    },
    Info {
        r#ref: Ref,
    },
    Install {
        r#ref: Ref,
    },
    Run {
        r#ref: Ref,
        #[clap(long, help = "Command to run instead of default")]
        command: Option<String>,
        args: Vec<String>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();

    let repo = Arc::new(composefs::repository::Repository::<Sha256HashValue>::open_user()?);
    match &args.command {
        Cmd::List => {
            let index = get_index(&args.repository)
                .await
                .with_context(|| format!("Fetching index from {}", args.repository))?;

            for r#ref in index.keys() {
                println!("{ref}");
            }
        }
        Cmd::Search { term } => {
            let index = get_index(&args.repository)
                .await
                .with_context(|| format!("Fetching index from {}", args.repository))?;

            let term = term.to_lowercase();

            for r#ref in index.keys() {
                if r#ref.as_ref().to_lowercase().contains(&term) {
                    println!("{ref}");
                }
            }
        }
        Cmd::Info { r#ref } => {
            let index = get_index(&args.repository)
                .await
                .with_context(|| format!("Fetching index from {}", args.repository))?;

            let Some((img, manifest)) = index.get(r#ref) else {
                bail!("No such ref {ref}");
            };

            println!("{}{}", &args.repository, &img);
            println!("{manifest:?}");
        }
        Cmd::Install { r#ref } => {
            let index = get_index(&args.repository)
                .await
                .with_context(|| format!("Fetching index from {}", args.repository))?;

            install::install(&repo, &args.repository, &index, r#ref).await?;
            println!("Now: run {ref}");
        }
        Cmd::Run {
            r#ref,
            command,
            args,
        } => {
            run_sandboxed(&repo, r#ref, command.as_deref(), args);
        }
    }

    Ok(())
}
