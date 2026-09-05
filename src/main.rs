use clap::Parser;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::ExitCode;

mod http;

#[derive(Debug, Parser)]
#[command(
    name = "specmesh-server",
    version,
    about = "Authenticated loopback Streamable HTTP MCP server for SpecMesh"
)]
struct Cli {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long, value_name = "PORT")]
    port: NonZeroU16,
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    token_file: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerOptions {
    pub workspace: Option<PathBuf>,
    pub port: u16,
    pub token_file: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    http::run(ServerOptions {
        workspace: cli.workspace,
        port: cli.port.get(),
        token_file: cli.token_file,
    })
}
