use anyhow::{Error, Result};
use node_semver::{Range, Version};

use crate::metadata::PackageMetadata;

fn satisfies_semver(semver: String, version: Version) -> bool {
    let range: Range = semver.parse().unwrap();

    range.satisfies(&version)
}

pub fn resolve_version(metadata: PackageMetadata, semver: String) -> Result<String> {
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
        .rfind(|version| satisfies_semver(semver.to_string(), version.clone()))
        .map(|value| value.to_string())
        .ok_or_else(|| {
            Error::msg(format!(
                "Failed to find a matching version for {}@{}",
                metadata.name, semver
            ))
        })
}
