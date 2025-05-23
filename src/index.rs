use std::{collections::HashMap, fs::create_dir_all, path::PathBuf};

use anyhow::{Context, Result};
use dirs::cache_dir;
use http_cache_reqwest::{CACacheManager, Cache, CacheMode, HttpCache, HttpCacheOptions};
use reqwest::{Client, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use serde::Deserialize;

use crate::r#ref::Ref;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct IndexResponse {
    results: Vec<Name>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Name {
    name: String,
    images: Vec<Image>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Image {
    digest: String,
    labels: Labels,
}

#[derive(Debug, Deserialize)]
struct Labels {
    #[serde(rename = "org.flatpak.ref")]
    r#ref: Ref,
    #[serde(rename = "org.flatpak.metadata")]
    metadata: String,
}

fn get_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "386",
        "x86_64" => "amd64",
        other => other,
    }
}

fn ensure_cache_path() -> Option<PathBuf> {
    let mut path = cache_dir()?;
    path.push("flatpak-next/http-cacache");
    create_dir_all(&path).ok()?;
    Some(path)
}

fn create_client() -> ClientWithMiddleware {
    let mut builder = ClientBuilder::new(Client::new());

    if let Some(path) = ensure_cache_path() {
        builder = builder.with(Cache(HttpCache {
            mode: CacheMode::Default,
            manager: CACacheManager { path },
            options: HttpCacheOptions::default(),
        }));
    }

    builder.build()
}

pub(crate) async fn get_index(repository: &str) -> Result<HashMap<Ref, (String, String)>> {
    let mut index = Url::parse(repository)?.join("index/static")?;

    let mut pairs = index.query_pairs_mut();
    pairs.append_pair("architecture", get_oci_arch());
    pairs.append_pair("label:org.flatpak.ref:exists", "1");
    pairs.append_pair("os", "linux");
    pairs.append_pair("tag", "latest");
    drop(pairs);

    let response: IndexResponse = create_client()
        .get(index)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("Parsing index JSON failed")?;

    let mut table = HashMap::new();

    for name in response.results {
        for image in name.images {
            table.insert(
                image.labels.r#ref,
                (
                    format!("{}@{}", name.name, image.digest),
                    image.labels.metadata,
                ),
            );
        }
    }

    Ok(table)
}
