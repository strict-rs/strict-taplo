use schemars::schema_for;
use taplo_common::config::Config;
use taplo_common::environment::LocalEnvironment;
use tokio::io::AsyncWriteExt as _;

use crate::CliError;
use crate::LocalCommandFuture;
use crate::Taplo;
use crate::args::ConfigCommand;
use crate::default_config;

/// Print the default configuration or its JSON schema.
pub(super) fn execute_config<E: LocalEnvironment>(
  taplo: &Taplo<E>,
  command: ConfigCommand,
) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    let mut stdout = taplo.env.stdout();
    match command {
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
  })
}
