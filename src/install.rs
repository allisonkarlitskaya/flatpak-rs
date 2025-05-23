use std::{collections::HashMap, sync::Arc};

use crate::{manifest::Manifest, r#ref::Ref};
use anyhow::{Result, bail};
use composefs::{fsverity::FsVerityHashValue, repository::Repository};
use rustix::fs::{AtFlags, unlinkat};

async fn install_one<ObjectID: FsVerityHashValue>(
    repo: &Arc<Repository<ObjectID>>,
    r#ref: &Ref,
    img_base: &str,
    img: &str,
) -> Result<String> {
    let mut img_ref = img_base.replace("https", "docker");
    img_ref.push_str(img);

    println!(">>> Downloading from {img_ref}");

    // HACK: We don't want to hear that we already have a reference with a given name, so unlink it
    // ahead of time in case it already exists... it's just a symlink (and the container config is
    // content addressed) so we won't actually redownload anything if we're already up to date...
    let _ = unlinkat(
        repo.objects_dir()?,
        format!("../streams/refs/flatpak-rs/{ref}"),
        AtFlags::empty(),
    );

    let (digest, verity) =
        composefs_oci::pull(repo, &img_ref, Some(&format!("flatpak-rs/{ref}"))).await?;

    println!("config {}", hex::encode(digest));
    println!("verity {}", verity.to_hex());

    let mut fs =
        composefs_oci::image::create_filesystem(repo, &hex::encode(digest), Some(&verity))?;
    let image_id = fs.commit_image(repo, None)?;

    println!("image {}", image_id.to_hex());

    Ok(hex::encode(digest))
}

pub async fn install<ObjectID: FsVerityHashValue>(
    repo: &Arc<Repository<ObjectID>>,
    img_base: &str,
    index: &HashMap<Ref, (String, String)>,
    r#ref: &Ref,
) -> Result<(Option<String>, String)> {
    let Some((img, manifest)) = index.get(r#ref) else {
        bail!("No such ref {ref}");
    };

    println!("First manifest {manifest:?}");
    let first = install_one(repo, r#ref, img_base, img).await?;

    let (app, runtime) = if r#ref.is_runtime() {
        (None, first)
    } else {
        let manifest = Manifest::new(manifest)?;
        let runtime = manifest.get_runtime()?;
        let Some((runtime_img, runtime_manifest)) = index.get(&runtime) else {
            bail!("No such ref {ref}");
        };

        println!("Linked runtime manifest {runtime_manifest:?}");
        let runtime = install_one(repo, &runtime, img_base, runtime_img).await?;
        (Some(first), runtime)
    };

    Ok((app, runtime))
}
