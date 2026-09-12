#![forbid(clippy::unwrap_used, unsafe_code)]
use tracing_subscriber::{
	EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt as _,
};

use crate::commands::backend_args;

mod api;
mod commands;
mod database;
mod paynow;
mod utils;

#[tokio::main]
async fn main() {
	// Setup logging
	tracing_subscriber::registry()
		.with(fmt::layer())
		.with(
			EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
		)
		.init();

	// Setup TLS
	rustls::crypto::ring::default_provider()
		.install_default()
		.expect("Failed to install rustls crypto provider");

	let args = backend_args().run();

	match args.command {
		commands::Subcommand::Serve(args) => api::start(args).await,
		commands::Subcommand::ProvisionPaynow(args) => {
			commands::provision::run(args).await;
		}
	}
}
