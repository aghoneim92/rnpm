use anyhow::Result;
use tokio::{fs::File, io::AsyncWriteExt};

use crate::{metadata::PackageJson, rnpm::PackageFetchResult};

pub async fn create_and_write_lockfile(
    package_json: PackageJson,
    package_fetch_result: PackageFetchResult,
) -> Result<()> {
    let mut file = File::create("rnpm-lock.yaml").await?;

    file.write(b"lockfileVersion: 5.3\n\nspecifiers:\n").await?;

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
        file.write(format!("  {}\n", constraint_string).as_bytes())
            .await?;
    }

    file.write(b"\ndependencies:\n").await?;

    let PackageFetchResult {
        resolved_dependencies,
        resolved_dev_dependencies,
    } = package_fetch_result;
    for (name, resolved_version) in resolved_dependencies {
        file.write(format!("  {}: {}\n", name, resolved_version).as_bytes())
            .await?;
    }

    file.write(b"\ndevDependencies:\n").await?;

    for (name, resolved_version) in resolved_dev_dependencies {
        file.write(format!("  {}: {}\n", name, resolved_version).as_bytes())
            .await?;
    }

    Ok(())
}
