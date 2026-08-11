use anyhow::Result;
use lsp_server::Connection;
use lsp_types::InitializeParams;

use ferry::lsp::{FerryOperations, Server};

fn main() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let initialization_params =
        match connection.initialize(serde_json::to_value(ferry::lsp::capabilities())?) {
            Ok(params) => params,
            Err(error) => {
                if error.channel_is_disconnected() {
                    io_threads.join()?;
                }
                return Err(error.into());
            }
        };
    let _params: InitializeParams = serde_json::from_value(initialization_params)?;

    ferry::lsp::main_loop(connection, Server::new(FerryOperations::new()?))?;
    io_threads.join()?;
    Ok(())
}
