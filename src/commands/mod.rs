use std::{net::SocketAddr, str::FromStr};

use axum_client_ip::ClientIpSource;
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
		fallback(SocketAddr::from_str("[::]:8080").expect("This str is always a valid SocketAddr"))
	)]
	pub(crate) bind_addr: SocketAddr,
	/// The Tebex webhook secret to validate all webhook endpoint signatures
	/// with
	#[bpaf(long("tebex-webhook-secret"), env("TEBEX_WEBHOOK_SECRET"))]
	pub(crate) tebex_webhook_secret: String,
	/// The Tebex game server secret to use for interacting with the Plugin API
	#[bpaf(long("tebex-game-server-secret"), env("TEBEX_GAME_SERVER_SECRET"))]
	pub(crate) tebex_game_server_secret: String,
	/// The URL to use for connecting to the database
	#[bpaf(long("database-url"), env("DATABASE_URL"))]
	pub(crate) database_url: String,
	/// Where to source client IPs from. By default, parsed IPs will simply be
	/// the connecting remote IP address. However, other options like
	/// RightmostXForwardedFor can be passed to change this behavior. When set
	/// to anything except ConnectInfo, make sure that the API is run behind a
	/// TRUSTED reverse proxy, and is not exposed to the internet otherwise.
	/// See https://docs.rs/axum-client-ip/latest/axum_client_ip/enum.ClientIpSource.html for availible choices.
	#[bpaf(
		long("client-ip-source"),
		env("CLIENT_IP_SOURCE"),
		fallback(ClientIpSource::ConnectInfo)
	)]
	pub(crate) client_ip_source: ClientIpSource,
	/// The name of the s3 bucket to use
	#[bpaf(long("s3-bucket-name"), env("S3_BUCKET_NAME"))]
	pub(crate) s3_bucket_name: String,
	/// The region of the s3 bucket to use
	#[bpaf(long("s3-bucket-region"), env("S3_BUCKET_REGION"))]
	pub(crate) s3_bucket_region: String,
	/// The endpoint of the s3 bucket to use
	#[bpaf(long("s3-bucket-endpoint"), env("S3_BUCKET_ENDPOINT"))]
	pub(crate) s3_bucket_endpoint: String
}
