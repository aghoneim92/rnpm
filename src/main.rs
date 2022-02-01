use async_recursion::async_recursion;
use futures::lock::Mutex;
use hyper::body::to_bytes;
use hyper::client::HttpConnector;
use hyper::{Body, Client, Method, Request, Uri};
use hyper_tls::HttpsConnector;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

mod semver;

use semver::Semver;

type MetadataDb = Arc<Mutex<HashMap<String, PackageMetadata>>>;

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

struct Rnpm {
    client: Client<HttpsConnector<HttpConnector>>,
    metadata_db: MetadataDb,
    dependency_node_map: HashMap<String, DependencyNode>,
}

type Error = Box<dyn std::error::Error + Send + Sync>;

impl Rnpm {
    fn new() -> Self {
        let connector = HttpsConnector::new();
        let client = Client::builder().build(connector);
        let metadata_db: MetadataDb = Arc::new(Mutex::new(HashMap::new()));
        let dependency_node_map = HashMap::new();
        Self {
            client,
            metadata_db,
            dependency_node_map,
        }
    }

    async fn fetch_metadata(&self, package_name: String) -> Result<PackageMetadata, Error> {
        println!("Fetching metadata for {}", package_name);
        let db = self.metadata_db.clone();
        let mut map = db.lock().await;
        if map.contains_key(&package_name) {
            println!("Metadata already fetched!");
            return Ok(map.get(&package_name).unwrap().clone());
        }

        let uri: Uri = format!("https://registry.npmjs.org/{}", package_name).parse()?;
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header("Accept", "application/vnd.npm.install-v1+json")
            .body(Body::empty())?;
        let mut resp = self.client.request(request).await?;
        let bytes = to_bytes(resp.body_mut()).await?;
        let resp = String::from_utf8(bytes.into_iter().collect())?;
        let metadata: PackageMetadata = serde_json::from_str(&resp)?;

        map.insert(package_name, metadata.clone());

        println!("Fetched and cached metadata..");

        Ok(metadata)
    }

    fn resolve_version(&self, metadata: PackageMetadata, semver: String) -> Option<String> {
        println!("Resolving version for {}@{}", metadata.name, semver);
        let dist_tags = metadata.dist_tags.clone();
        let found_version = dist_tags.get(&semver);
        if let Some(version) = found_version {
            return Some(version.to_string());
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
    }

    #[async_recursion]
    async fn resolve_dependencies(
        &mut self,
        dependencies: HashMap<String, String>,
    ) -> Result<(), Error> {
        for (name, semver) in dependencies {
            let metadata = self.fetch_metadata(name.clone()).await?;
            let resolved_version = self
                .resolve_version(metadata.clone(), semver.clone())
                .expect("msg");
            let version_metadata = metadata.versions.get(&resolved_version).expect(&format!(
                "Couldn't find resolved version {resolved_version} for {name}@{semver}",
                resolved_version = resolved_version,
                name = name,
                semver = semver
            ));
            self.recurse_node(DependencyNode {
                name: name,
                version: resolved_version,
                dependencies: version_metadata.dependencies.clone(),
                dev_dependencies: version_metadata.dev_dependencies.clone(),
                peer_dependencies: version_metadata.peer_dependencies.clone(),
            })
            .await?;
        }
        Ok(())
    }

    #[async_recursion]
    async fn recurse_node(&mut self, dependency_node: DependencyNode) -> Result<(), Error> {
        let version_string = format!("{}@{}", dependency_node.name, dependency_node.version);
        if !self.dependency_node_map.contains_key(&version_string) {
            self.dependency_node_map
                .insert(version_string, dependency_node.clone());
            if let Some(dependencies) = dependency_node.dependencies {
                self.resolve_dependencies(dependencies).await?;
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let package_json: PackageJson =
        serde_json::from_reader(File::open("package.json").unwrap()).unwrap();
    let mut rnpm = Rnpm::new();
    let dependency_node = DependencyNode {
        name: package_json.name,
        version: package_json.version,
        dependencies: package_json.dependencies.clone(),
        dev_dependencies: package_json.dev_dependencies.clone(),
        peer_dependencies: package_json.peer_dependencies.clone(),
    };
    rnpm.recurse_node(dependency_node).await?;
    println!("Done!");
    Ok(())
}
