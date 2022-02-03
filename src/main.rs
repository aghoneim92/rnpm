use anyhow::{Error, Result};
use async_recursion::async_recursion;
use bytes::Bytes;
use console::Term;
use deadqueue::unlimited;
use futures::future::join_all;
use log::{debug, log_enabled, Level};
use moka::future::Cache;
use node_semver::Version;
use reqwest::header::HeaderMap;
use reqwest::{Client, ClientBuilder, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::join;
use tokio::sync::Mutex;
use waitmap::WaitMap;

mod semver;

use semver::Semver;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PackageJson {
    name: String,
    version: String,
    dependencies: Option<HashMap<String, String>>,
    peer_dependencies: Option<HashMap<String, String>>,
    dev_dependencies: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct DistMetadata {
    shasum: String,
    tarball: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct VersionMetadata {
    name: String,
    version: String,
    dist: DistMetadata,
    dependencies: Option<HashMap<String, String>>,
    peer_dependencies: Option<HashMap<String, String>>,
    dev_dependencies: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PackageMetadata {
    name: String,
    #[serde(rename = "dist-tags")]
    dist_tags: HashMap<String, String>,
    versions: HashMap<String, VersionMetadata>,
}

#[derive(Clone)]
struct Rnpm {
    client: Client,
    base_url: Url,
    metadata_cache: Cache<String, PackageMetadata>,
    resolved_cache: Cache<String, ()>,
    metadata_wait_map: Arc<WaitMap<String, PackageMetadata>>,
    metadata_request_queue: Arc<unlimited::Queue<String>>,
    tarball_request_queue: Arc<unlimited::Queue<String>>,
    total: Arc<AtomicU32>,
    term: Term,
    lock_file: Arc<Mutex<File>>,
}

impl Rnpm {
    fn new(
        metadata_request_queue: Arc<deadqueue::unlimited::Queue<String>>,
        tarball_request_queue: Arc<unlimited::Queue<String>>,
        metadata_wait_map: Arc<WaitMap<String, PackageMetadata>>,
        lock_file: Arc<Mutex<File>>,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert("Accept", "application/vnd.npm.install-v1+json".parse()?);
        headers.insert("Connection", "Keep-Alive".parse()?);
        let client = ClientBuilder::new()
            .default_headers(headers)
            .gzip(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        let base_url = Url::parse(
            /* "https://registry.npmjs.org/", */ "http://localhost:4873/",
        )?;
        let metadata_cache = Cache::new(10_000);
        let resolved_cache = Cache::new(10_000);
        let total = Arc::new(AtomicU32::new(0));
        let term = Term::stdout();
        Ok(Self {
            client,
            base_url,
            metadata_cache,
            resolved_cache,
            metadata_wait_map,
            metadata_request_queue,
            tarball_request_queue,
            total,
            term,
            lock_file,
        })
    }
}

fn resolve_version(metadata: PackageMetadata, semver: String) -> Result<String> {
    let dist_tags = metadata.dist_tags.clone();
    let found_version = dist_tags.get(&semver);
    if let Some(version) = found_version {
        return Ok(version.to_string());
    }

    let mut versions: Vec<Version> = metadata
        .versions
        .keys()
        .map(|key| key.parse::<Version>().unwrap())
        .collect();
    versions.sort_unstable();

    versions
        .into_iter()
        .rfind(|version| Semver::satisfies_semver(semver.to_string(), version.clone()))
        .map(|value| value.to_string())
        .ok_or_else(|| {
            Error::msg(format!(
                "Failed to find a matching version for {}@{}",
                metadata.name, semver
            ))
        })
}

async fn fetch_metadata(rnpm: Rnpm, package_name: String) -> Result<PackageMetadata> {
    let url = rnpm.base_url.join(&package_name)?;
    debug!("Fetching {}", url.as_str());
    let metadata: PackageMetadata = rnpm.client.get(url).send().await?.json().await?;

    Ok(metadata)
}

#[async_recursion]
async fn get_dependency_node(
    rnpm: Rnpm,
    name: String,
    constraint: String,
) -> Result<(String, String)> {
    let package_metadata = if let Some(package_metadata) = rnpm.metadata_cache.get(&name) {
        package_metadata
    } else {
        rnpm.metadata_request_queue.push(name.clone());
        let package_metadata = rnpm
            .metadata_wait_map
            .wait(&name)
            .await
            .unwrap()
            .value()
            .clone();
        rnpm.metadata_cache
            .insert(name.clone(), package_metadata.clone())
            .await;

        package_metadata
    };

    let resolved_version = resolve_version(package_metadata.clone(), constraint)?;
    let version_string = format!("{}@{}", name, resolved_version);
    if let Some(()) = rnpm.resolved_cache.get(&version_string) {
        return Ok((name, resolved_version));
    }
    rnpm.resolved_cache.insert(version_string, ()).await;
    rnpm.total.fetch_add(1, Ordering::SeqCst);
    if !log_enabled!(Level::Error) {
        rnpm.term.clear_last_lines(1)?;
        rnpm.term
            .write_line(&format!("Total: {}", rnpm.total.load(Ordering::SeqCst)))?;
    }
    let version_metadata = package_metadata
        .versions
        .get(&resolved_version)
        .unwrap()
        .clone();
    rnpm.tarball_request_queue
        .push(version_metadata.dist.tarball);

    let mut fetches = vec![];
    if let Some(dependencies) = version_metadata.dependencies {
        for (name, constraint) in dependencies {
            fetches.push(get_dependency_node(rnpm.clone(), name, constraint));
        }
    }

    let results = futures::future::join_all(fetches).await;
    for result in results {
        result?;
    }

    Ok((name, resolved_version))
}

async fn recurse_package(rnpm: Rnpm, package_json: PackageJson) -> Result<()> {
    let mut dependency_fetches = vec![];
    let mut dev_dependency_fetches = vec![];
    if let Some(dependencies) = package_json.dependencies {
        for (name, constraint) in dependencies {
            dependency_fetches.push(get_dependency_node(rnpm.clone(), name, constraint));
        }
    }
    if let Some(dev_dependencies) = package_json.dev_dependencies {
        for (name, constraint) in dev_dependencies {
            dev_dependency_fetches.push(get_dependency_node(rnpm.clone(), name, constraint));
        }
    }

    let (dependency_results, dev_dependency_results) = join!(
        join_all(dependency_fetches),
        join_all(dev_dependency_fetches)
    );
    rnpm.lock_file
        .lock()
        .await
        .write(b"\ndependencies:\n")
        .await?;
    let mut resolved_dependencies = vec![];
    for result in dependency_results {
        resolved_dependencies.push(result?);
    }
    resolved_dependencies.sort_unstable_by_key(|dep| dep.0.clone());
    for (name, resolved_version) in resolved_dependencies {
        rnpm.lock_file
            .lock()
            .await
            .write(format!("  {}: {}\n", name, resolved_version).as_bytes())
            .await?;
    }

    rnpm.lock_file
        .lock()
        .await
        .write(b"\ndevDependencies:\n")
        .await?;
    resolved_dependencies = vec![];
    for result in dev_dependency_results {
        resolved_dependencies.push(result?);
    }
    resolved_dependencies.sort_unstable_by_key(|dep| dep.0.clone());
    for (name, resolved_version) in resolved_dependencies {
        rnpm.lock_file
            .lock()
            .await
            .write(format!("  {}: {}\n", name, resolved_version).as_bytes())
            .await?;
    }

    Ok(())
}

async fn fetch_metadata_task(
    queue: Arc<unlimited::Queue<String>>,
    rnpm: Rnpm,
    wait_map: Arc<WaitMap<String, PackageMetadata>>,
) -> Result<()> {
    loop {
        let message = queue.pop().await;
        if message == "KILL" {
            return Ok(());
        }
        debug!("Fetching metadata for {}", message);
        let metadata = fetch_metadata(rnpm.clone(), message.clone()).await?;
        debug!("Fetched metadata for {}", message);
        wait_map.insert(message, metadata);
    }
}

async fn fetch_tarball(rnpm: Rnpm, tarball_url: String) -> Result<Bytes> {
    debug!("Fetching tarball {}", tarball_url);
    let bytes = rnpm
        .client
        .get(tarball_url.clone())
        .send()
        .await?
        .bytes()
        .await?;

    debug!("Fetched tarball {}", tarball_url);

    Ok(bytes)
}

async fn fetch_tarball_task(queue: Arc<unlimited::Queue<String>>, rnpm: Rnpm) -> Result<()> {
    loop {
        let message = queue.pop().await;
        if message == "KILL" {
            return Ok(());
        }
        fetch_tarball(rnpm.clone(), message.clone()).await?;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    flexi_logger::Logger::try_with_env()?.start()?;
    let file = std::fs::File::open("package.json")?;
    let mut lock_file = File::create("rnpm-lock.yaml").await?;
    lock_file
        .write(b"lockfileVersion: 5.3\n\nspecifiers:\n")
        .await?;

    let package_json: PackageJson = serde_json::from_reader(file)?;
    let mut sorted_dependencies: Vec<(String, String)> = vec![];
    if let Some(dependencies) = package_json.dependencies.clone() {
        sorted_dependencies.append(&mut dependencies.into_iter().collect());
    }
    if let Some(dev_dependencies) = package_json.dev_dependencies.clone() {
        sorted_dependencies.append(&mut dev_dependencies.into_iter().collect());
    }

    sorted_dependencies.sort_unstable_by_key(|entry| entry.0.clone());
    for (name, constraint) in sorted_dependencies {
        let constraint_string = format!("{}: {}", name, constraint);
        lock_file
            .write(format!("  {}\n", constraint_string).as_bytes())
            .await?;
    }

    let metadata_wait_map = Arc::new(WaitMap::new());
    let metadata_request_queue = Arc::new(unlimited::Queue::new());
    let tarball_request_queue = Arc::new(unlimited::Queue::new());
    let rnpm = Rnpm::new(
        metadata_request_queue.clone(),
        tarball_request_queue.clone(),
        metadata_wait_map.clone(),
        Arc::new(Mutex::new(lock_file)),
    )?;
    let metadata_task_handles = (0..8).map(|_| {
        let queue = metadata_request_queue.clone();
        let rnpm = rnpm.clone();
        let wait_map = metadata_wait_map.clone();
        tokio::spawn(fetch_metadata_task(queue, rnpm, wait_map))
    });
    let metadata_task_results = futures::future::join_all(metadata_task_handles);
    let tarball_task_handles = (0..8).map(|_| {
        let queue = tarball_request_queue.clone();
        let rnpm = rnpm.clone();
        tokio::spawn(fetch_tarball_task(queue, rnpm))
    });
    let tarball_task_results = futures::future::join_all(tarball_task_handles);
    if !log_enabled!(Level::Error) {
        rnpm.term.write_line("Total: 0")?;
    }
    recurse_package(rnpm.clone(), package_json).await?;
    for _ in 0..8 {
        metadata_request_queue.push("KILL".to_string());
        tarball_request_queue.push("KILL".to_string());
    }
    let results = metadata_task_results.await;
    for result in results {
        result??;
    }
    let results = tarball_task_results.await;
    for result in results {
        result??;
    }

    Ok(())
}
