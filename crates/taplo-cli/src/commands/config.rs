use schemars::schema_for;
use taplo_common::config::Config;
use taplo_common::environment::Environment;
use tokio::io::AsyncWriteExt;

use crate::Taplo;
use crate::args::ConfigCommand;
use crate::default_config;

impl<E: Environment> Taplo<E> {
  pub async fn execute_config(&self, cmd: ConfigCommand) -> Result<(), anyhow::Error> {
    let mut stdout = self.env.stdout();
    match cmd {
      ConfigCommand::Default => {
        stdout.write_all(toml::to_string_pretty(&default_config())?.as_bytes()).await?;
        stdout.flush().await?;
        Ok(())
      }
      ConfigCommand::Schema => {
        stdout
          .write_all(serde_json::to_string_pretty(&schema_for!(Config))?.as_bytes())
          .await?;
        stdout.flush().await?;
        Ok(())
      }
    }
  }
}
