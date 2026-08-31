pub(crate) mod provision;

use std::{net::SocketAddr, str::FromStr};

use axum_client_ip::ClientIpSource;
use bpaf::Bpaf;
use http::{HeaderValue, header::InvalidHeaderValue};

#[derive(Clone, Debug, Bpaf)]
#[bpaf(options, version)]
pub(crate) struct BackendArgs {
	#[bpaf(external(self::subcommand))]
	pub(crate) command: Subcommand,
}

#[derive(Clone, Debug, Bpaf)]
#[expect(
	clippy::large_enum_variant,
	reason = "this is only used in one place, after which the variant value is moved"
)]
pub(crate) enum Subcommand {
	#[bpaf(command("serve"))]
	Serve(#[bpaf(external(serve_args))] ServeArgs),
	#[bpaf(command("provision-paynow"))]
	ProvisionPaynow(#[bpaf(external(provision_paynow_args))] ProvisionPaynowArgs),
}

#[derive(Clone, Debug, Bpaf)]
pub(crate) struct ProvisionPaynowArgs {
	#[bpaf(long("database-url"), env("DATABASE_URL"), argument("URL"))]
	pub(crate) database_url: String,
	#[bpaf(long("paynow-api-key"), env("PAYNOW_API_KEY"), argument("KEY"))]
	pub(crate) paynow_api_key: String,
	#[bpaf(long("paynow-store-id"), env("PAYNOW_STORE_ID"), argument("ID"))]
	pub(crate) paynow_store_id: String,
	#[bpaf(
		long("paynow-api-base"),
		env("PAYNOW_API_BASE"),
		argument("URL"),
		fallback(crate::paynow::DEFAULT_API_BASE.to_string())
	)]
	pub(crate) paynow_api_base: String,
	/// Report what would be created without writing anything.
	#[bpaf(long("dry-run"), switch)]
	pub(crate) dry_run: bool,
	/// Also push the current price of every already provisioned product,
	/// repairing drift left behind by a failed price update.
	#[bpaf(long("sync-prices"), switch)]
	pub(crate) sync_prices: bool,
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
		argument::<String>("ADDR"),
		parse(parse_bind_addrs),
		fallback_with(default_bind_addrs)
	)]
	pub(crate) bind_addr: Vec<SocketAddr>,
	/// The PayNow management API key used to create checkouts and products
	#[bpaf(long("paynow-api-key"), env("PAYNOW_API_KEY"))]
	pub(crate) paynow_api_key: String,
	/// The id of the PayNow store to sell from
	#[bpaf(long("paynow-store-id"), env("PAYNOW_STORE_ID"))]
	pub(crate) paynow_store_id: String,
	/// The PayNow webhook signing secret used to validate webhook signatures
	#[bpaf(long("paynow-webhook-secret"), env("PAYNOW_WEBHOOK_SECRET"))]
	pub(crate) paynow_webhook_secret: String,
	/// The URL PayNow redirects the buyer to after a successful checkout
	#[bpaf(long("paynow-return-url"), env("PAYNOW_RETURN_URL"))]
	pub(crate) paynow_return_url: String,
	/// The URL PayNow redirects the buyer to if they cancel checkout
	#[bpaf(long("paynow-cancel-url"), env("PAYNOW_CANCEL_URL"))]
	pub(crate) paynow_cancel_url: String,
	/// The base URL of the PayNow API, overridable to point at a local stub
	#[bpaf(
		long("paynow-api-base"),
		env("PAYNOW_API_BASE"),
		fallback(crate::paynow::DEFAULT_API_BASE.to_string())
	)]
	pub(crate) paynow_api_base: String,
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
	pub(crate) s3_bucket_endpoint: String,
	/// Password for admin operations
	#[bpaf(long("admin-password"), env("ADMIN_PASSWORD"))]
	pub(crate) admin_password: String,
	#[bpaf(
		long("render-service-url"),
		env("RENDER_SERVICE_URL"),
		fallback(String::new())
	)]
	pub(crate) render_service_url: String,
	/// The origins allowed to make cross-origin requests to the API, comma
	/// seperated.
	#[bpaf(
		long("cors-origins"),
		env("CORS_ORIGINS"),
		argument::<String>("ORIGINS"),
		parse(parse_cors_origins),
		fallback_with(default_cors_origins)
	)]
	pub(crate) cors_origins: Vec<HeaderValue>,
	#[bpaf(
		long("oidc-issuer"),
		env("OIDC_ISSUER"),
		fallback("https://plus.polyfrost.org".to_string())
	)]
	pub(crate) oidc_issuer: String,
	#[bpaf(
		long("special-chat-target"),
		env("SPECIAL_CHAT_TARGETS"),
		argument::<String>("UUIDS"),
		parse(parse_special_chat_targets),
		fallback(Vec::new())
	)]
	pub(crate) special_chat_targets: Vec<uuid::Uuid>,
	#[bpaf(
		long("special-chat-auto-reply"),
		env("SPECIAL_CHAT_AUTO_REPLY"),
		argument::<String>("MESSAGE"),
		optional
	)]
	pub(crate) special_chat_auto_reply: Option<String>,
}

fn parse_bind_addrs(value: String) -> Result<Vec<SocketAddr>, std::net::AddrParseError> {
	value
		.split(',')
		.map(str::trim)
		.filter(|addr| !addr.is_empty())
		.map(SocketAddr::from_str)
		.collect()
}

fn default_bind_addrs() -> Result<Vec<SocketAddr>, std::net::AddrParseError> {
	parse_bind_addrs("[::]:8080,0.0.0.0:8080".to_owned())
}

fn parse_cors_origins(value: String) -> Result<Vec<HeaderValue>, InvalidHeaderValue> {
	value
		.split(',')
		.map(str::trim)
		.filter(|origin| !origin.is_empty())
		.map(HeaderValue::from_str)
		.collect()
}

fn default_cors_origins() -> Result<Vec<HeaderValue>, InvalidHeaderValue> {
	parse_cors_origins(
		"https://plus-admin.polyfrost.org,http://localhost:3000".to_owned(),
	)
}

fn parse_special_chat_targets(value: String) -> Result<Vec<uuid::Uuid>, uuid::Error> {
	value
		.split(',')
		.map(str::trim)
		.filter(|uuid| !uuid.is_empty())
		.map(uuid::Uuid::parse_str)
		.collect()
}
