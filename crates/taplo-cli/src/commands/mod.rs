use taplo_common::environment::Environment;

use crate::Taplo;
use crate::args::TaploArgs;
use crate::args::TaploCommand;

mod config;
mod format;
#[cfg(feature = "lint")]
mod lint;
#[cfg(feature = "lsp")]
mod lsp;
mod queries;

#[cfg(feature = "toml-test")]
mod toml_test;

impl<E: Environment> Taplo<E> {
  pub async fn execute(&mut self, taplo: TaploArgs) -> Result<(), anyhow::Error> {
    self.colors = match taplo.colors {
      crate::args::Colors::Auto => self.env.atty_stderr(),
      crate::args::Colors::Always => true,
      crate::args::Colors::Never => false,
    };

    match taplo.cmd {
      #[cfg(feature = "completions")]
      TaploCommand::Completions {
        shell,
      } => {
        use std::io::stdout;
        use std::str::FromStr;

        use anyhow::anyhow;
        use clap::CommandFactory;
        use clap_complete::generate;
        use clap_complete::shells::Shell;

        let shell = Shell::from_str(&shell).map_err(|e| anyhow!(e))?;
        generate(
          shell,
          &mut TaploArgs::command(),
          TaploArgs::command().get_bin_name().unwrap(),
          &mut stdout(),
        );
        Ok(())
      }
      TaploCommand::Config {
        cmd,
      } => self.execute_config(cmd).await,
      TaploCommand::Format(fmt) => self.execute_format(fmt).await,
      TaploCommand::Get(cmd) => self.execute_get(cmd).await,
      #[cfg(feature = "lint")]
      TaploCommand::Lint(cmd) => self.execute_lint(cmd).await,
      #[cfg(feature = "lsp")]
      TaploCommand::Lsp {
        cmd,
      } => self.execute_lsp(cmd).await,
      #[cfg(feature = "toml-test")]
      TaploCommand::TomlTest {} => self.execute_toml_test().await,
    }
  }
}
