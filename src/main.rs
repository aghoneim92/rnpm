mod lock_file;
mod metadata;
mod rnpm;
mod semver;

use anyhow::Result;
use rnpm::Rnpm;

#[tokio::main]
async fn main() -> Result<()> {
    flexi_logger::Logger::try_with_env()?.start()?;

    let rnpm = Rnpm::new().await?;
    rnpm.install().await?;

    Ok(())
}
