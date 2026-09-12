use aide::axum::ApiRouter;
use axum::{
	Json,
	extract::{Path, State},
	http::{Method, StatusCode},
	response::{IntoResponse, Response},
	routing::get,
};
use serde_json::Value;
use tracing::warn;

use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor},
	paynow::{PayNowError, Retry},
};

/// Only these paths are reachable. Anything else the key can do — customers,
/// orders, refunds — stays out of reach of the admin password.
const COLLECTIONS: [&str; 3] = ["sales", "coupons", "affiliate-links"];
/// Store-wide settings: one object, so no id and no create or delete.
const SETTINGS: [&str; 1] = ["upsell-settings"];

#[derive(Debug, thiserror::Error)]
enum ProxyError {
	#[error("Unknown store resource {0}")]
	UnknownResource(String),
	#[error("{0}")]
	PayNow(#[from] PayNowError),
}

impl IntoResponse for ProxyError {
	fn into_response(self) -> Response {
		let status = match &self {
			ProxyError::UnknownResource(_) => StatusCode::NOT_FOUND,
			// PayNow rejecting the body is the admin's mistake, not an outage.
			ProxyError::PayNow(error) if error.is_client_error() => {
				StatusCode::BAD_REQUEST
			}
			ProxyError::PayNow(_) => StatusCode::BAD_GATEWAY,
		};

		crate::api::error_response(status, self)
	}
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.route("/{resource}", get(list).post(create).patch(patch_settings))
		.route("/{resource}/{id}", get(read).patch(update).delete(remove))
}

async fn list(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(resource): Path<String>,
) -> Result<Response, ProxyError> {
	let suffix = collection_or_settings(&resource)?;
	forward(&state, Method::GET, &suffix, None, Retry::Idempotent).await
}

async fn create(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(resource): Path<String>,
	Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
	// Settings are patched, never created.
	let suffix = collection(&resource)?;
	forward(
		&state,
		Method::POST,
		&suffix,
		Some(&body),
		Retry::ConnectOnly,
	)
	.await
}

async fn read(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path((resource, id)): Path<(String, String)>,
) -> Result<Response, ProxyError> {
	let suffix = member(&resource, &id)?;
	forward(&state, Method::GET, &suffix, None, Retry::Idempotent).await
}

async fn update(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path((resource, id)): Path<(String, String)>,
	Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
	let suffix = member(&resource, &id)?;
	forward(
		&state,
		Method::PATCH,
		&suffix,
		Some(&body),
		Retry::Idempotent,
	)
	.await
}

async fn remove(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path((resource, id)): Path<(String, String)>,
) -> Result<Response, ProxyError> {
	let suffix = member(&resource, &id)?;
	forward(&state, Method::DELETE, &suffix, None, Retry::Idempotent).await
}

/// A settings object is patched on its collection path, so it answers here too.
async fn patch_settings(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(resource): Path<String>,
	Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
	let suffix = settings(&resource)?;
	forward(
		&state,
		Method::PATCH,
		&suffix,
		Some(&body),
		Retry::Idempotent,
	)
	.await
}

fn collection(resource: &str) -> Result<String, ProxyError> {
	if COLLECTIONS.contains(&resource) {
		Ok(format!("/{resource}"))
	} else {
		Err(ProxyError::UnknownResource(resource.to_owned()))
	}
}

fn settings(resource: &str) -> Result<String, ProxyError> {
	if SETTINGS.contains(&resource) {
		Ok(format!("/{resource}"))
	} else {
		Err(ProxyError::UnknownResource(resource.to_owned()))
	}
}

fn collection_or_settings(resource: &str) -> Result<String, ProxyError> {
	collection(resource).or_else(|_| settings(resource))
}

fn member(resource: &str, id: &str) -> Result<String, ProxyError> {
	// Ids are flake ids; anything else would be an attempt to walk the path.
	if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
		return Err(ProxyError::UnknownResource(resource.to_owned()));
	}

	collection(resource).map(|suffix| format!("{suffix}/{id}"))
}

async fn forward(
	state: &ApiState,
	method: Method,
	suffix: &str,
	body: Option<&Value>,
	retry: Retry,
) -> Result<Response, ProxyError> {
	let bytes = state
		.paynow
		.client
		.forward(method, suffix, body, retry)
		.await?;

	if bytes.is_empty() {
		return Ok(StatusCode::NO_CONTENT.into_response());
	}

	match serde_json::from_slice::<Value>(&bytes) {
		Ok(value) => Ok(Json(value).into_response()),
		Err(error) => {
			warn!(suffix, "PayNow returned a body we could not parse: {error}");
			Err(ProxyError::PayNow(PayNowError::Decode(error)))
		}
	}
}
