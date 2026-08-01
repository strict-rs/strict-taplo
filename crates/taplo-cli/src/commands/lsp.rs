//! Native concurrent language-server command.

use std::time::Duration;

use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::LocalEnvironment;
use taplo_common::schema::transport::concurrent_http_client;
use taplo_lsp::create_concurrent_server;
use taplo_lsp::create_concurrent_world;

use crate::CliError;
use crate::LocalCommandFuture;
use crate::Taplo;
use crate::args::LspCommand;
use crate::args::LspCommandIo;

/// Execute the native concurrent LSP server with this command's actual host environment.
pub(super) fn execute_lsp<E>(taplo: &mut Taplo<E>, command: LspCommand) -> LocalCommandFuture<'_, Result<(), CliError>>
where
  E: LocalEnvironment + ConcurrentEnvironment,
  E::Stdin: Send,
  E::Stdout: Send,
{
  Box::pin(async move {
    let config = taplo.load_config(&command.general).await?;
    let environment = taplo.env.clone();
    let http = concurrent_http_client(&taplo.env, Duration::from_secs(5))?;
    let server = create_concurrent_server();
    let world = create_concurrent_world(environment, http)?;
    world.set_default_config(config);
    world.set_default_cache_path(command.general.cache_path.clone());
    let shutdown = async_ctrlc::CtrlC::new().map_err(|error| CliError::ShutdownSignal {
      message: error.to_string(),
    })?;

    match command.io {
      LspCommandIo::Tcp {
        address,
      } => server.listen_tcp(world, &address, shutdown).await?,
      LspCommandIo::Stdio {} => {
        server
          .listen_stdio(world, taplo.env.stdin(), taplo.env.stdout(), shutdown)
          .await?;
      }
    }
    Ok(())
  })
}
