use node_semver::{Range, Version};

pub struct Semver;

impl Semver {
    pub fn satisfies_semver(semver: String, version: String) -> bool {
        let version: Version = version.parse().unwrap();
        let range: Range = semver.parse().unwrap();

        range.satisfies(&version)
    }
}
