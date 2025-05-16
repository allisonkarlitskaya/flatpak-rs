mod index;
mod manifest;
mod r#ref;

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use composefs::{
    fsverity::{FsVerityHashValue, Sha256HashValue},
    repository::Repository,
};

use crate::{index::get_index, r#ref::Ref, manifest::Manifest};

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
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    List,
    Search { term: String },
    Info { r#ref: Ref },
    Install { r#ref: Ref },
    Run { r#ref: Ref },
}

async fn install_one<ObjectID: FsVerityHashValue>(
    repo: &Arc<Repository<ObjectID>>,
    img_base: &str,
    img: &str,
) -> Result<()> {
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

    Ok(())
}

async fn install<ObjectID: FsVerityHashValue>(
    repo: &Arc<Repository<ObjectID>>,
    img_base: &str,
    index: &HashMap<Ref, (String, Manifest)>,
    r#ref: &Ref,
) -> Result<()> {
    let Some((img, manifest)) = index.get(r#ref) else {
        bail!("No such ref {ref}");
    };

    println!("App manifest {manifest:?}");
    install_one(repo, img_base, img).await?;

    if r#ref.is_app() {
        let runtime = manifest.get_runtime()?;
        let Some((runtime_img, runtime_manifest)) = index.get(&runtime) else {
            bail!("No such ref {ref}");
        };

        println!("Runtime manifest {runtime_manifest:?}");
        install_one(repo, img_base, runtime_img).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let repo = Arc::new(composefs::repository::Repository::<Sha256HashValue>::open_user()?);

    let index = get_index(&args.repository)
        .await
        .with_context(|| format!("Fetching index from {}", args.repository))?;

    match &args.command {
        Command::List => {
            for r#ref in index.keys() {
                println!("{ref}");
            }
        }
        Command::Search { term } => {
            let term = term.to_lowercase();

            for r#ref in index.keys() {
                if r#ref.as_ref().to_lowercase().contains(&term) {
                    println!("{ref}");
                }
            }
        }
        Command::Info { r#ref } => {
            let Some((img, manifest)) = index.get(r#ref) else {
                bail!("No such ref {ref}");
            };

            println!("{}{}", &args.repository, &img);
            println!("{manifest:?}");
        }
        Command::Install { r#ref } => {
            install(&repo, &args.repository, &index, r#ref).await?;
        }
        Command::Run { r#ref } => {
            install(&repo, &args.repository, &index, r#ref).await?;
        }
    }

    Ok(())
}
