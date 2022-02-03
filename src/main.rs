use async_recursion::async_recursion;
use hyper::body::to_bytes;
use hyper::client::HttpConnector;
use hyper::{Body, Client, Method, Request, Uri};
use hyper_tls::HttpsConnector;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;

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

#[derive(Clone, Debug)]
struct DependencyNode {
    name: String,
    version: String,
    dependencies: Option<HashMap<String, String>>,
    peer_dependencies: Option<HashMap<String, String>>,
    dev_dependencies: Option<HashMap<String, String>>,
}

#[derive(Debug)]
struct RnpmError {
    message: String,
}

impl std::fmt::Display for RnpmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RnpmError {}
unsafe impl Send for RnpmError {}
unsafe impl Sync for RnpmError {}

type HttpsClient = Client<HttpsConnector<HttpConnector>>;

#[derive(Clone)]
struct Rnpm {
    client: HttpsClient,
    metadata_cache: Cache<String, PackageMetadata>,
    dependency_node_cache: Cache<String, DependencyNode>,
}

type Error = Box<dyn std::error::Error + Send + Sync>;

async fn fetch_metadata(rnpm: Rnpm, package_name: String) -> Result<PackageMetadata, Error> {
    println!("Fetching metadata for {}", package_name);
    let metadata = rnpm.metadata_cache.get(&package_name);
    if let Some(metadata) = metadata {
        println!("Metadata already fetched for {}", package_name);
        return Ok(metadata);
    }

    let uri: Uri = format!("https://registry.npmjs.org/{}", package_name).parse()?;
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Accept", "application/vnd.npm.install-v1+json")
        .body(Body::empty())?;
    let mut resp = rnpm.client.request(request).await?;
    let bytes = to_bytes(resp.body_mut()).await?;
    let resp = String::from_utf8(bytes.into_iter().collect())?;
    let metadata: PackageMetadata = serde_json::from_str(&resp)?;

    rnpm.metadata_cache
        .insert(package_name.clone(), metadata.clone())
        .await;

    println!("Fetched and cached metadata for {}", package_name);

    Ok(metadata)
}

#[async_recursion]
async fn resolve_dependencies(
    rnpm: Rnpm,
    package_name: String,
    dependencies: HashMap<String, String>,
) -> Result<(), Error> {
    println!("Resolving dependencies for {}", package_name);
    let metadata_futs = dependencies
        .iter()
        .map(|(name, _)| tokio::spawn(fetch_metadata(rnpm.clone(), name.clone())));
    let results = futures::future::join_all(metadata_futs).await;
    let mut recursion_futs = vec![];
    for result in results {
        let metadata = result??;
        let semver = dependencies.get(&metadata.name).ok_or_else(|| -> Error {
            Box::new(RnpmError {
                message: format!(
                    "Failed to find dependency with name {} in {}",
                    metadata.name, package_name
                ),
            })
        })?;
        let resolved_version = resolve_version(metadata.clone(), semver.clone())?;
        let version_metadata =
            metadata
                .versions
                .get(&resolved_version)
                .ok_or_else(|| -> Error {
                    Box::new(RnpmError {
                        message: format!(
                            "Failed to find version for {}@{}",
                            metadata.name, resolved_version
                        ),
                    })
                })?;
        recursion_futs.push(tokio::spawn(recurse_node(
            rnpm.clone(),
            DependencyNode {
                name: metadata.name,
                version: resolved_version,
                dependencies: version_metadata.dependencies.clone(),
                dev_dependencies: version_metadata.dev_dependencies.clone(),
                peer_dependencies: version_metadata.peer_dependencies.clone(),
            },
        )));
    }
    let results = futures::future::join_all(recursion_futs).await;
    for result in results {
        result??;
    }
    Ok(())
}

#[async_recursion]
async fn recurse_node(rnpm: Rnpm, dependency_node: DependencyNode) -> Result<(), Error> {
    let version_string = format!("{}@{}", dependency_node.name, dependency_node.version);
    println!("Recursing package {}", version_string);
    let cached_dependency_node = rnpm.dependency_node_cache.get(&version_string);
    if let None = cached_dependency_node {
        rnpm.dependency_node_cache
            .insert(version_string, dependency_node.clone())
            .await;
        if let Some(dependencies) = dependency_node.dependencies {
            resolve_dependencies(rnpm.clone(), dependency_node.name.clone(), dependencies).await?;
        }
    } else {
        println!("Package {} already visited!", version_string);
    }
    Ok(())
}

fn resolve_version(metadata: PackageMetadata, semver: String) -> Result<String, Error> {
    println!("Resolving version for {}@{}", metadata.name, semver);
    let dist_tags = metadata.dist_tags.clone();
    let found_version = dist_tags.get(&semver);
    if let Some(version) = found_version {
        return Ok(version.to_string());
    }

    let mut versions: Vec<String> = metadata
        .versions
        .keys()
        .map(|key| key.to_string())
        .collect();
    versions.sort_unstable();

    versions
        .iter()
        .rfind(|val| Semver::satisfies_semver(semver.to_string(), val.to_string()))
        .map(|value| value.to_string())
        .ok_or_else(|| -> Error {
            Box::new(RnpmError {
                message: format!(
                    "Failed to find a matching version for {}@{}",
                    metadata.name, semver
                ),
            })
        })
}

impl Rnpm {
    fn new() -> Self {
        let connector = HttpsConnector::new();
        let client = Client::builder().build(connector);
        let metadata_cache = Cache::new(10_100);
        let dependency_node_cache = Cache::new(10_000);
        Self {
            client,
            metadata_cache,
            dependency_node_cache,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let file = File::open("package.json")?;
    let package_json: PackageJson = serde_json::from_reader(file)?;
    let rnpm: Rnpm = Rnpm::new();
    let dependency_node = DependencyNode {
        name: package_json.name,
        version: package_json.version,
        dependencies: package_json.dependencies.clone(),
        dev_dependencies: package_json.dev_dependencies.clone(),
        peer_dependencies: package_json.peer_dependencies.clone(),
    };
    recurse_node(rnpm, dependency_node).await?;
    println!("Done!");
    Ok(())
}
