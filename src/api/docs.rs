use aide::{openapi::OpenApi, redoc::Redoc, scalar::Scalar, swagger::Swagger};
use axum::{
	http::header,
	response::Html,
	routing::{MethodRouter, get},
};

/// An API version with its own OpenAPI document and documentation pages.
#[derive(Debug, Clone, Copy)]
pub(super) struct DocVersion {
	number: u8,
	base: &'static str,
}

pub(super) const V0: DocVersion = DocVersion {
	number: 0,
	base: "",
};

pub(super) const V1: DocVersion = DocVersion {
	number: 1,
	base: "/v1",
};

/// A documentation UI that every version is rendered into.
#[derive(Debug, Clone, Copy)]
pub(super) enum DocPage {
	Scalar,
	Swagger,
	Redoc,
}

impl DocVersion {
	/// How the version is referred to in prose, e.g. `v0`.
	pub(super) fn label(self) -> String {
		format!("v{}", self.number)
	}

	pub(super) fn document_version(self) -> String {
		self.number.to_string()
	}

	pub(super) fn title(self) -> String {
		format!("Poly+ API ({})", self.label())
	}

	/// Path this version's OpenAPI document is served from.
	pub(super) fn spec_url(self) -> String {
		format!("{}/openapi.json", self.base)
	}

	/// Path one of this version's documentation pages is served from.
	pub(super) fn page_url(self, page: DocPage) -> String {
		format!("{}/{}", self.base, page.name())
	}
}

impl DocPage {
	/// Every page, so that each version can be mounted into all of them.
	pub(super) const ALL: &'static [Self] = &[Self::Scalar, Self::Swagger, Self::Redoc];

	fn name(self) -> &'static str {
		match self {
			Self::Scalar => "scalar",
			Self::Swagger => "swagger",
			Self::Redoc => "redoc",
		}
	}
}

/// Serves an already rendered OpenAPI document as JSON.
pub(super) fn spec_route<S>(spec: &OpenApi) -> MethodRouter<S>
where
	S: Clone + Send + Sync + 'static,
{
	let json = serde_json::to_string(spec)
		.expect("Unable to render OpenAPI documentation as JSON")
		.into_boxed_str();

	get(move || async move { ([(header::CONTENT_TYPE, "application/json")], json) })
}

/// Serves the given documentation page for the given version.
pub(super) fn page_route<S>(version: DocVersion, page: DocPage) -> MethodRouter<S>
where
	S: Clone + Send + Sync + 'static,
{
	let spec_url = version.spec_url();
	let title = version.title();

	let html = match page {
		DocPage::Scalar => Scalar::new(spec_url).with_title(&title).html(),
		DocPage::Swagger => Swagger::new(spec_url).with_title(&title).html(),
		DocPage::Redoc => Redoc::new(spec_url).with_title(&title).html(),
	};

	let html: &'static str = Box::leak(html.into_boxed_str());

	get(move || async move { Html(html) })
}
