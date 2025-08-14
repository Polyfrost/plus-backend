use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::apis::plugin::PluginApiRequest;

/// GET https://plugin.tebex.io/player/:id/packages?package=<package>
///
/// Fetches all active packages for a given player ID. This will return 200 on
/// any VALID player ID (i.e. any real Minecraft player UUID), but will return
/// 404 if an invalid ID is passed.
pub struct ActivePackagesRequest<'a> {
	/// The ID of the user to fetch active packages for
	pub id: &'a str,
	/// Optionally, a package to filter by (to check if a specific package has
	/// been purchased)
	pub package: Option<&'a str>
}

/// Information about a payment related to an active package for a player
#[derive(Debug, Deserialize, Clone)]
pub struct ActivePackage {
	pub txn_id: String,
	pub date: DateTime<Utc>,
	pub quantity: u32,
	pub package: ActivePackageInfo
}

/// Information about an active package for a player
#[derive(Debug, Deserialize, Clone)]
pub struct ActivePackageInfo {
	pub id: u32,
	pub name: String
}

// TODO: use thiserror enums for returning errors. probably an API-wide enum
// with Reqwest::error & 403 invalid secret error, and then more specific enums
// per-request
impl PluginApiRequest for ActivePackagesRequest<'_> {
	type Response = Result<Vec<ActivePackage>, reqwest::Error>;

	async fn fetch(self, client: &super::TebexPluginApiClient) -> Self::Response {
		let req = client
			.inner
			.get(format!(
				"https://plugin.tebex.io/player/{id}/packages",
				id = self.id
			))
			.header("User-Agent", crate::apis::USER_AGENT)
			.header("X-Tebex-Secret", &client.secret);

		let req = if let Some(package) = self.package {
			req.query(&[("package", package)])
		} else {
			req
		};

		req.send().await?.error_for_status()?.json().await
	}
}
