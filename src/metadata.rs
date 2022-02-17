use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PackageJson {
    pub name: String,
    pub version: String,
    pub dependencies: Option<HashMap<String, String>>,
    pub peer_dependencies: Option<HashMap<String, String>>,
    pub dev_dependencies: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PackageMetadata {
    pub name: String,
    #[serde(rename = "dist-tags")]
    pub dist_tags: HashMap<String, String>,
    pub versions: HashMap<String, VersionMetadata>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VersionMetadata {
    pub name: String,
    pub version: String,
    pub dist: DistMetadata,
    pub dependencies: Option<HashMap<String, String>>,
    pub peer_dependencies: Option<HashMap<String, String>>,
    pub dev_dependencies: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DistMetadata {
    pub shasum: String,
    pub tarball: String,
}
