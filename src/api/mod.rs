pub(crate) mod admin_auth;
mod docs;
mod state;
mod v0;
mod v1;

use aide::{
	axum::ApiRouter,
	openapi::{
		ApiKeyLocation, Contact, License, OpenApi, SchemaObject, SecurityScheme, Server,
	},
	transform::TransformOpenApi,
};
use axum::{
	Extension,
	http::{Method, header},
};
use schemars::{JsonSchema, schema_for};
use tokio::net::TcpListener;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
	api::{
		docs::{DocPage, DocVersion},
		state::ApiState,
		v0::websocket::structs::{ClientBoundPacket, ServerBoundPacket},
	},
	commands::ServeArgs,
};

fn init_openapi_spec<'a>(
	spec: TransformOpenApi<'a>,
	version: DocVersion,
) -> TransformOpenApi<'a> {
	spec.version(&version.document_version())
		.title(&version.title())
		.summary("An API used as the backend of the Poly+ mod")
		.description(&format!(
			"This API provides all the backend services necessary for enabling the \
			 functionalities of the Poly+ mod, such as storing and serving cosmetic \
			 information.\n\nThis document only covers the {} endpoints; the other \
			 versions are documented separately.",
			version.label()
		))
		.license(License {
			name: "PolyForm Shield License 1.0.0".to_string(),
			url: Some("https://polyformproject.org/licenses/shield/1.0.0/".to_string()),
			..Default::default()
		})
		.tos("https://polyfrost.org/legal/terms/")
		.contact(Contact {
			url: Some("https://polyfrost.org/contact/".to_string()),
			email: Some("contact@atmofrost.org".to_string()),
			..Default::default()
		})
		.server(Server {
			url: "https://plus.polyfrost.org".to_string(),
			description: Some("The production Poly+ backend".to_string()),
			..Default::default()
		})
		.server(Server {
			url: "https://plus-staging.polyfrost.org".to_string(),
			description: Some("The staging Poly+ backend".to_string()),
			..Default::default()
		})
		.server(Server {
			url: "http://localhost:8080".to_string(),
			description: Some("A development backend server".to_string()),
			..Default::default()
		})
		.security_scheme(
			v0::account::OPENAPI_SECURITY_NAME,
			SecurityScheme::Http {
				scheme: "bearer".to_string(),
				bearer_format: Some("paseto".to_string()),
				description: None,
				extensions: Default::default(),
			},
		)
		.security_scheme(
			"Admin Password",
			SecurityScheme::ApiKey {
				location: ApiKeyLocation::Header,
				name: "Authorization".to_string(),
				description: Some("Admin password".to_string()),
				extensions: Default::default(),
			},
		)
}

pub(crate) async fn start(args: ServeArgs) {
	let state = ApiState::new(&args).await;

	let mut openapi_v0 = OpenApi::default();
	let v0 = v0::setup_router()
		.await
		.with_state(state.clone())
		.finish_api_with(&mut openapi_v0, |spec| init_openapi_spec(spec, docs::V0));

	// Manually add documentation for websockets because aide doesn't detect them
	if let Some(components) = openapi_v0.components.as_mut() {
		components.schemas.insert(
			ClientBoundPacket::schema_name().into_owned(),
			SchemaObject {
				json_schema: schema_for!(ClientBoundPacket),
				example: None,
				external_docs: None,
			},
		);
		components.schemas.insert(
			ServerBoundPacket::schema_name().into_owned(),
			SchemaObject {
				json_schema: schema_for!(ServerBoundPacket),
				example: None,
				external_docs: None,
			},
		);
	}

	let mut openapi_v1 = OpenApi::default();
	let v1 = ApiRouter::new()
		.nest("/v1", v1::setup_router().await)
		.with_state(state)
		.finish_api_with(&mut openapi_v1, |spec| init_openapi_spec(spec, docs::V1));

	// Final router object
	let mut app = v0.merge(v1);

	// Add visual doc pages
	for (version, openapi) in [(docs::V0, &openapi_v0), (docs::V1, &openapi_v1)] {
		app = app.route(&version.spec_url(), docs::spec_route(openapi));

		for &page in DocPage::ALL {
			app = app.route(&version.page_url(page), docs::page_route(version, page));
		}
	}

	// Add middleware
	let app = app
		.layer(Extension(args.client_ip_source))
		.layer(TraceLayer::new_for_http())
		.layer(
			CorsLayer::new()
				.allow_origin(args.cors_origins)
				.allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
				.allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
		);

	let mut listeners = Vec::new();
	for addr in &args.bind_addr {
		match TcpListener::bind(addr).await {
			Ok(listener) => {
				listeners.push(listener);
				tracing::info!(%addr, "binded to address");
			}
			Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
				tracing::warn!(%addr, %err, "skipping bind address, already in use");
			}
			Err(err) => panic!("Unable to bind on {addr}: {err}"),
		}
	}
	assert!(!listeners.is_empty(), "No socket addresses could be bound");

	let make_service = app.into_make_service();
	let servers = listeners.into_iter().map(|listener| {
		let make_service = make_service.clone();
		tokio::spawn(async move { axum::serve(listener, make_service).await })
	});

	futures::future::try_join_all(servers)
		.await
		.expect("infailable: server tasks should not be cancelled")
		.into_iter()
		.for_each(|result| result.expect("infailable: axum::serve never returns"));
}
