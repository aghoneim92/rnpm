use node_semver::{Range, Version};

pub struct Semver;

impl Semver {
    pub fn satisfies_semver(semver: String, version: Version) -> bool {
        let range: Range = semver.parse().unwrap();

        range.satisfies(&version)
    }
}
