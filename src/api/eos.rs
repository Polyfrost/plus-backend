use std::{
	fmt,
	future::Future,
	net::IpAddr,
	sync::Arc,
	time::{Duration, Instant},
};

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

use crate::commands::ServeArgs;

const EPIC_API: &str = "https://api.epicgames.dev";
const EPIC_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);
const OPENID_PROVIDER: &str = "openid";
const BACKGROUND_RETRIES: u32 = 3;

#[derive(Clone)]
pub(in crate::api) struct EosClient {
	client_id: String,
	client_secret: String,
	deployment_id: String,
	token: Arc<Mutex<Option<CachedToken>>>,
}

struct CachedToken {
	access_token: String,
	refresh_at: Instant,
}

impl fmt::Debug for EosClient {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EosClient")
			.field("client_id", &self.client_id)
			.field("deployment_id", &self.deployment_id)
			.finish_non_exhaustive()
	}
}

#[derive(thiserror::Error, Debug)]
pub(in crate::api) enum EosError {
	#[error("Unable to reach Epic Online Services: {0}")]
	Request(#[from] reqwest::Error),
	#[error("Epic Online Services returned no participant token")]
	MissingParticipant,
}

#[derive(Deserialize)]
struct TokenResponse {
	access_token: String,
	expires_in: u64,
}

#[derive(Deserialize)]
struct AccountsResponse {
	#[serde(default)]
	ids: std::collections::HashMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomTokenRequest<'a> {
	participants: [RoomParticipant<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomParticipant<'a> {
	puid: &'a str,
	#[serde(skip_serializing_if = "Option::is_none")]
	client_ip: Option<String>,
	hard_muted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomTokenResponse {
	client_base_url: String,
	#[serde(default)]
	participants: Vec<IssuedParticipant>,
}

#[derive(Deserialize)]
struct IssuedParticipant {
	token: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HardMuteRequest {
	hard_muted: bool,
}

#[derive(Debug, Clone)]
pub(in crate::api) struct RoomToken {
	pub(in crate::api) client_base_url: String,
	pub(in crate::api) participant_token: String,
}

impl EosClient {
	pub(in crate::api) fn from_args(args: &ServeArgs) -> Option<Self> {
		let configured = !args.eos_trusted_client_id.is_empty()
			&& !args.eos_trusted_client_secret.is_empty()
			&& !args.eos_deployment_id.is_empty();

		configured.then(|| Self {
			client_id: args.eos_trusted_client_id.clone(),
			client_secret: args.eos_trusted_client_secret.expose().to_owned(),
			deployment_id: args.eos_deployment_id.clone(),
			token: Arc::default(),
		})
	}

	async fn access_token(&self, http: &Client) -> Result<String, EosError> {
		let mut cached = self.token.lock().await;
		if let Some(token) = cached.as_ref()
			&& Instant::now() < token.refresh_at
		{
			return Ok(token.access_token.clone());
		}

		let response: TokenResponse = http
			.post(format!("{EPIC_API}/auth/v1/oauth/token"))
			.timeout(EPIC_TIMEOUT)
			.basic_auth(&self.client_id, Some(&self.client_secret))
			.form(&[("grant_type", "client_credentials")])
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		let lifetime =
			Duration::from_secs(response.expires_in).saturating_sub(TOKEN_REFRESH_MARGIN);
		*cached = Some(CachedToken {
			access_token: response.access_token.clone(),
			refresh_at: Instant::now() + lifetime,
		});

		Ok(response.access_token)
	}

	pub(in crate::api) async fn product_user_id(
		&self,
		http: &Client,
		account_id: &str,
	) -> Result<Option<String>, EosError> {
		let token = self.access_token(http).await?;

		let response: AccountsResponse = http
			.get(format!("{EPIC_API}/user/v1/accounts"))
			.timeout(EPIC_TIMEOUT)
			.bearer_auth(token)
			.query(&[
				("accountId", account_id),
				("identityProviderId", OPENID_PROVIDER),
			])
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		Ok(response.ids.get(account_id).cloned())
	}

	pub(in crate::api) async fn create_room_token(
		&self,
		http: &Client,
		room: &str,
		puid: &str,
		client_ip: Option<IpAddr>,
		hard_muted: bool,
	) -> Result<RoomToken, EosError> {
		let token = self.access_token(http).await?;

		let response: RoomTokenResponse = http
			.post(self.room_url(room))
			.timeout(EPIC_TIMEOUT)
			.bearer_auth(token)
			.json(&RoomTokenRequest {
				participants: [RoomParticipant {
					puid,
					client_ip: client_ip.map(|ip| ip.to_canonical().to_string()),
					hard_muted,
				}],
			})
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		let participant = response
			.participants
			.into_iter()
			.next()
			.ok_or(EosError::MissingParticipant)?;

		Ok(RoomToken {
			client_base_url: response.client_base_url,
			participant_token: participant.token,
		})
	}

	pub(in crate::api) async fn remove_participant(
		&self,
		http: &Client,
		room: &str,
		puid: &str,
	) -> Result<(), EosError> {
		let token = self.access_token(http).await?;

		let response = http
			.delete(self.participant_url(room, puid))
			.timeout(EPIC_TIMEOUT)
			.bearer_auth(token)
			.send()
			.await?;
		if response.status() == StatusCode::NOT_FOUND {
			return Ok(());
		}
		response.error_for_status()?;

		Ok(())
	}

	pub(in crate::api) async fn set_hard_muted(
		&self,
		http: &Client,
		room: &str,
		puid: &str,
		hard_muted: bool,
	) -> Result<(), EosError> {
		let token = self.access_token(http).await?;

		let response = http
			.post(self.participant_url(room, puid))
			.timeout(EPIC_TIMEOUT)
			.bearer_auth(token)
			.json(&HardMuteRequest { hard_muted })
			.send()
			.await?;
		if response.status() == StatusCode::NOT_FOUND {
			return Ok(());
		}
		response.error_for_status()?;

		Ok(())
	}

	fn room_url(&self, room: &str) -> String {
		format!("{EPIC_API}/rtc/v1/{}/room/{room}", self.deployment_id)
	}

	fn participant_url(&self, room: &str, puid: &str) -> String {
		format!("{}/participants/{puid}", self.room_url(room))
	}
}

pub(in crate::api) fn spawn_with_retries<F, Fut>(action: &'static str, mut attempt: F)
where
	F: FnMut() -> Fut + Send + 'static,
	Fut: Future<Output = Result<(), EosError>> + Send,
{
	tokio::spawn(async move {
		let mut delay = Duration::from_secs(1);
		for tries in 0..=BACKGROUND_RETRIES {
			match attempt().await {
				Ok(()) => return,
				Err(error) if tries == BACKGROUND_RETRIES => {
					warn!("Giving up on EOS {action}: {error}");
				}
				Err(error) => {
					warn!("EOS {action} failed, retrying: {error}");
					tokio::time::sleep(delay).await;
					delay *= 2;
				}
			}
		}
	});
}
