pub mod plugin;

const USER_AGENT: &str = concat!(
	env!("CARGO_PKG_NAME"),
	"/",
	env!("CARGO_PKG_VERSION"),
	" (",
	env!("CARGO_PKG_REPOSITORY"),
	", rust library)"
);
