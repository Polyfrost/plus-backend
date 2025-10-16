mod account;
mod cosmetics;
mod payments;
mod state;
mod websocket;

use std::net::IpAddr;

use aide::{
	axum::ApiRouter, openapi::{Contact, License, OpenApi, SecurityScheme, Server}, redoc::Redoc, scalar::Scalar, swagger::Swagger, transform::TransformOpenApi, OperationIo
};
use axum::{
	Extension,
	extract::FromRequestParts,
	http::{header, request::Parts},
	routing::get as axum_get
};
use axum_client_ip::ClientIp;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use crate::{api::state::ApiState, commands::ServeArgs};

#[derive(OperationIo)]
pub struct ClientIpExtractor(pub IpAddr);

impl<S: Sync> FromRequestParts<S> for ClientIpExtractor {
	type Rejection = <ClientIp as FromRequestParts<S>>::Rejection;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &S
	) -> Result<Self, Self::Rejection> {
		ClientIp::from_request_parts(parts, state)
			.await
			.map(|ClientIp(ip)| Self(ip))
	}
}

fn init_openapi_spec(spec: TransformOpenApi<'_>) -> TransformOpenApi<'_> {
	spec.version("1.0.0")
		.title("Poly+ API")
		.summary("An API used as the backend of the Poly+ mod")
		.description(
			"This API provides all the backend services necessary for enabling the \
			 functionalities of the Poly+ mod, such as storing and serving cosmetic \
			 information."
		)
		.license(License {
			name: "PolyForm Shield License 1.0.0".to_string(),
			url: Some("https://polyformproject.org/licenses/shield/1.0.0/".to_string()),
			..Default::default()
		})
		.tos("https://polyfrost.org/legal/terms/")
		.contact(Contact {
			url: Some("https://polyfrost.org/contact/".to_string()),
			email: Some("ty@polyfrost.org".to_string()),
			..Default::default()
		})
		.server(Server {
			url: "https://plus.polyfrost.org".to_string(),
			description: Some("The production Poly+ backend".to_string()),
			..Default::default()
		})
		.server(Server {
			url: "http://localhost:8080".to_string(),
			description: Some("A development backend server".to_string()),
			..Default::default()
		})
		.security_scheme(account::OPENAPI_SECURITY_NAME, SecurityScheme::Http {
			scheme: "bearer".to_string(),
			bearer_format: Some("paseto".to_string()),
			description: None,
			extensions: Default::default()
		})
}

#[derive(Clone, Copy)]
struct OpenApiSpec(&'static str);

pub(crate) async fn start(args: ServeArgs) {
	let state = ApiState::new(&args).await;

	let app = ApiRouter::new()
		.nest("/payments", payments::setup_router().await)
		.nest("/account", account::setup_router().await)
		.nest("/cosmetics", cosmetics::setup_router().await)
		.merge(websocket::setup_router().await)
		.with_state(state);

	// Convert OpenAPI router to normal actix router, and render the doc as JSON
	let mut openapi = OpenApi::default();
	let app = app.finish_api_with(&mut openapi, init_openapi_spec);
	let openapi_rendered = Box::leak(
		serde_json::to_string(&openapi)
			.expect("Unable to render OpenAPI documentation as JSON")
			.into_boxed_str()
	);
	let app = app
		.route("/scalar", Scalar::new("/openapi.json").axum_route().into())
		.route("/swagger", Swagger::new("/openapi.json").axum_route().into())
		.route("/redoc", Redoc::new("/openapi.json").axum_route().into())
		.route(
			"/openapi.json",
			axum_get(
				async |Extension(OpenApiSpec(spec)): Extension<OpenApiSpec>| {
					([(header::CONTENT_TYPE, "application/json")], spec)
				}
			)
		)
		.layer(Extension(OpenApiSpec(openapi_rendered)))
		.layer(Extension(args.client_ip_source))
		.layer(TraceLayer::new_for_http());

	// Setup the listener and start the web server
	let listener = TcpListener::bind(args.bind_addr)
		.await
		.expect("Unable to bind on specififed socket address");

	axum::serve(listener, app.into_make_service())
		.await
		.expect("infailable: axum::serve never returns")
}
