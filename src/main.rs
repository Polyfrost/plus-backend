use crate::commands::backend_args;

mod api;
mod commands;

#[tokio::main]
async fn main() {
	let args = backend_args().run();

	match args.command {
		commands::Subcommand::Serve(args) => api::start(args).await
	}
}
