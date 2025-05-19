mod index;
mod manifest;
mod r#ref;
mod sandbox;

use std::{collections::HashMap, sync::Arc};

use crate::{index::get_index, manifest::Manifest, r#ref::Ref, sandbox::run_sandboxed};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use composefs::{
    fsverity::{FsVerityHashValue, Sha256HashValue},
    repository::Repository,
};

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
    Enter {
        runtime: String,
        app: Option<String>,
    },
}

async fn install_one<ObjectID: FsVerityHashValue>(
    repo: &Arc<Repository<ObjectID>>,
    img_base: &str,
    img: &str,
) -> Result<String> {
    let mut img_ref = img_base.replace("https", "docker");
    img_ref.push_str(img);

    println!(">>> Downloading from {img_ref}");

    let (digest, verity) = composefs_oci::pull(repo, &img_ref, None).await?;

    println!("config {}", hex::encode(digest));
    println!("verity {}", verity.to_hex());

    // TODO: use verity
    let mut fs = composefs_oci::image::create_filesystem(repo, &hex::encode(digest), None)?;
    let image_id = fs.commit_image(repo, None)?;

    println!("image {}", image_id.to_hex());

    Ok(hex::encode(digest))
}

async fn install<ObjectID: FsVerityHashValue>(
    repo: &Arc<Repository<ObjectID>>,
    img_base: &str,
    index: &HashMap<Ref, (String, Manifest)>,
    r#ref: &Ref,
) -> Result<(Option<String>, String)> {
    let Some((img, manifest)) = index.get(r#ref) else {
        bail!("No such ref {ref}");
    };

    println!("First manifest {manifest:?}");
    let first = install_one(repo, img_base, img).await?;

    let (app, runtime) = if r#ref.is_runtime() {
        (None, first)
    } else {
        let runtime = manifest.get_runtime()?;
        let Some((runtime_img, runtime_manifest)) = index.get(&runtime) else {
            bail!("No such ref {ref}");
        };

        println!("Linked runtime manifest {runtime_manifest:?}");
        let runtime = install_one(repo, img_base, runtime_img).await?;
        (Some(first), runtime)
    };

    Ok((app, runtime))
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

            let (app, runtime) = install(&repo, &args.repository, &index, r#ref).await?;
            println!("Now: enter {runtime} {}", app.as_deref().unwrap_or(""));
        }
        Cmd::Enter { runtime, app } => {
            run_sandboxed(app.as_deref(), runtime, &repo, "/bin/sh", &[]);
        }
    }

    Ok(())
}
