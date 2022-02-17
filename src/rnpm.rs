use anyhow::Result;
use async_recursion::async_recursion;
use bytes::Bytes;
use console::Term;
use deadqueue::unlimited;
use futures::future::join_all;
use log::{debug, log_enabled, Level};
use moka::future::Cache;
use reqwest::header::HeaderMap;
use reqwest::{Client, ClientBuilder, Url};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::join;
use waitmap::WaitMap;

use crate::lock_file::create_and_write_lockfile;
use crate::metadata::*;
use crate::semver::resolve_version;

#[derive(Clone)]
pub struct Rnpm {
    client: Client,
    base_url: Url,
    metadata_cache: Cache<String, PackageMetadata>,
    resolved_cache: Cache<String, ()>,
    metadata_wait_map: Arc<WaitMap<String, PackageMetadata>>,
    metadata_request_queue: Arc<unlimited::Queue<String>>,
    tarball_request_queue: Arc<unlimited::Queue<String>>,
    total: Arc<AtomicU32>,
    term: Term,
}

pub struct PackageFetchResult {
    pub resolved_dependencies: Vec<(String, String)>,
    pub resolved_dev_dependencies: Vec<(String, String)>,
}

impl Rnpm {
    pub async fn new() -> Result<Self> {
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
        let metadata_wait_map = Arc::new(WaitMap::new());
        let metadata_request_queue = Arc::new(unlimited::Queue::new());
        let tarball_request_queue = Arc::new(unlimited::Queue::new());
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
        })
    }

    pub async fn install(&self) -> Result<()> {
        let file = std::fs::File::open("package.json")?;

        let package_json: PackageJson = serde_json::from_reader(file)?;

        let metadata_task_handles = (0..8).map(|_| {
            let queue = self.metadata_request_queue.clone();
            let rnpm = self.clone();
            let wait_map = self.metadata_wait_map.clone();
            tokio::spawn(fetch_metadata_task(queue, rnpm, wait_map))
        });
        let metadata_task_results = futures::future::join_all(metadata_task_handles);
        let tarball_task_handles = (0..8).map(|_| {
            let queue = self.tarball_request_queue.clone();
            let rnpm = self.clone();
            tokio::spawn(fetch_tarball_task(queue, rnpm))
        });
        let tarball_task_results = futures::future::join_all(tarball_task_handles);
        if !log_enabled!(Level::Error) {
            self.term.write_line("Total: 0")?;
        }

        let package_fetch_result = self.fetch_package(package_json.clone()).await?;
        create_and_write_lockfile(package_json, package_fetch_result).await?;
        for _ in 0..8 {
            self.metadata_request_queue.push("KILL".to_string());
            self.tarball_request_queue.push("KILL".to_string());
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

    async fn fetch_package(&self, package_json: PackageJson) -> Result<PackageFetchResult> {
        let mut dependency_fetches = vec![];
        let mut dev_dependency_fetches = vec![];
        if let Some(dependencies) = package_json.dependencies {
            for (name, constraint) in dependencies {
                dependency_fetches.push(self.get_dependency_node(name, constraint));
            }
        }
        if let Some(dev_dependencies) = package_json.dev_dependencies {
            for (name, constraint) in dev_dependencies {
                dev_dependency_fetches.push(self.get_dependency_node(name, constraint));
            }
        }

        let (dependency_results, dev_dependency_results) = join!(
            join_all(dependency_fetches),
            join_all(dev_dependency_fetches)
        );
        let mut resolved_dependencies = vec![];
        for result in dependency_results {
            resolved_dependencies.push(result?);
        }
        resolved_dependencies.sort_unstable_by_key(|dep| dep.0.clone());

        let mut resolved_dev_dependencies = vec![];
        for result in dev_dependency_results {
            resolved_dev_dependencies.push(result?);
        }
        resolved_dev_dependencies.sort_unstable_by_key(|dep| dep.0.clone());

        Ok(PackageFetchResult {
            resolved_dependencies,
            resolved_dev_dependencies,
        })
    }

    #[async_recursion]
    async fn get_dependency_node(
        &self,
        name: String,
        constraint: String,
    ) -> Result<(String, String)> {
        let package_metadata = if let Some(package_metadata) = self.metadata_cache.get(&name) {
            package_metadata
        } else {
            self.metadata_request_queue.push(name.clone());
            let package_metadata = self
                .metadata_wait_map
                .wait(&name)
                .await
                .unwrap()
                .value()
                .clone();
            self.metadata_cache
                .insert(name.clone(), package_metadata.clone())
                .await;

            package_metadata
        };

        let resolved_version = resolve_version(package_metadata.clone(), constraint)?;
        let version_string = format!("{}@{}", name, resolved_version);
        if let Some(()) = self.resolved_cache.get(&version_string) {
            return Ok((name, resolved_version));
        }
        self.resolved_cache.insert(version_string, ()).await;
        self.total.fetch_add(1, Ordering::SeqCst);
        if !log_enabled!(Level::Error) {
            self.term.clear_last_lines(1)?;
            self.term
                .write_line(&format!("Total: {}", self.total.load(Ordering::SeqCst)))?;
        }
        let version_metadata = package_metadata
            .versions
            .get(&resolved_version)
            .unwrap()
            .clone();
        self.tarball_request_queue
            .push(version_metadata.dist.tarball);

        let mut fetches = vec![];
        if let Some(dependencies) = version_metadata.dependencies {
            for (name, constraint) in dependencies {
                fetches.push(self.get_dependency_node(name, constraint));
            }
        }

        let results = futures::future::join_all(fetches).await;
        for result in results {
            result?;
        }

        Ok((name, resolved_version))
    }
}

async fn fetch_metadata(rnpm: Rnpm, package_name: String) -> Result<PackageMetadata> {
    let url = rnpm.base_url.join(&package_name)?;
    debug!("Fetching {}", url.as_str());
    let metadata: PackageMetadata = rnpm.client.get(url).send().await?.json().await?;

    Ok(metadata)
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
