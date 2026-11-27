#[cfg(feature = "nameplate-engine")]
mod linked;

#[cfg(feature = "nameplate-engine")]
pub(crate) use linked::{engine_issue, skipped};

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssueLevel {
	Error,
	Warning,
	Info,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct EngineIssue {
	pub(crate) code: String,
	pub(crate) level: IssueLevel,
	pub(crate) path: String,
	pub(crate) values: BTreeMap<String, f64>,
	pub(crate) names: BTreeMap<String, String>,
}

#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub(crate) struct EngineError(pub(crate) String);

pub(crate) trait NameplateEngine {
	fn validate(bundle: &[u8]) -> Vec<EngineIssue>;

	fn cover(
		bundle: &[u8],
		variant_key: Option<&str>,
	) -> Result<Option<Vec<u8>>, EngineError>;
}

#[cfg_attr(feature = "nameplate-engine", allow(dead_code))]
pub(crate) struct Unlinked;

impl NameplateEngine for Unlinked {
	fn validate(_bundle: &[u8]) -> Vec<EngineIssue> {
		Vec::new()
	}

	fn cover(
		_bundle: &[u8],
		_variant_key: Option<&str>,
	) -> Result<Option<Vec<u8>>, EngineError> {
		Ok(None)
	}
}

#[cfg(not(feature = "nameplate-engine"))]
pub(crate) type Engine = Unlinked;

#[cfg(feature = "nameplate-engine")]
pub(crate) type Engine = linked::Core;
