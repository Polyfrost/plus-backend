#![feature(macro_metavar_expr_concat)]
use tracing_subscriber::{
	EnvFilter,
	fmt,
	layer::SubscriberExt,
	util::SubscriberInitExt as _
};

use crate::commands::backend_args;

mod api;
mod commands;

#[tokio::main]
async fn main() {
	// Setup logging
	tracing_subscriber::registry()
		.with(fmt::layer())
		.with(EnvFilter::from_default_env())
		.init();

	let args = backend_args().run();

	match args.command {
		commands::Subcommand::Serve(args) => api::start(args).await
	}
}
