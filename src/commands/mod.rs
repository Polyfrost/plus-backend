use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use bpaf::Bpaf;

#[derive(Clone, Debug, Bpaf)]
#[bpaf(options, version)]
pub(crate) struct BackendArgs {
	#[bpaf(external(self::subcommand))]
	pub(crate) command: Subcommand
}

#[derive(Clone, Debug, Bpaf)]
pub(crate) enum Subcommand {
	#[bpaf(command("serve"))]
	Serve(#[bpaf(external(serve_args))] ServeArgs)
}

#[derive(Clone, Debug, Bpaf)]
pub(crate) struct ServeArgs {
	/// The socket addresses to bind the HTTP server to, comma seperated.
	/// If specified on the command line, multiple flags can be provided instead
	/// of passing a comma-delimited value.
	#[bpaf(
		long("bind-addr"),
		long("bind-address"),
		env("BIND_ADDR"),
		fallback(SocketAddr::from_str("[::]:8080").unwrap())
	)]
	pub(crate) bind_addr: SocketAddr,
	/// testing1
	#[bpaf(external)]
	pub(crate) tebex_webhook_secret: TebexWebhookSecret
}

#[derive(Clone, Debug, Bpaf)]
pub(crate) enum TebexWebhookSecret {
	/// testing2
	File(PathBuf),
	Raw(String)
}
