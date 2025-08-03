use aide::{
	axum::{ApiRouter, routing::post},
	openapi::{Info, OpenApi},
	scalar::Scalar
};
use axum::{Extension, http::header, routing::get as axum_get};
use tokio::net::TcpListener;

use crate::commands::ServeArgs;

#[derive(Clone, Copy)]
struct OpenApiSpec(&'static str);

pub(crate) async fn start(args: ServeArgs) {
	let app = ApiRouter::new().api_route(
		"/tebex-webhook-test",
		post(async |body: String| {
			let parsed = serde_json::from_str::<serde_json::Value>(&body).unwrap();
			println!("{}", serde_json::to_string_pretty(&parsed).unwrap());

			format!(
				r#"{{"id":"{}"}}"#,
				parsed
					.as_object()
					.unwrap()
					.get("id")
					.unwrap()
					.as_str()
					.unwrap()
			)
		})
	);

	// TODO: Fill this out
	let mut openapi = OpenApi {
		info: Info {
			description: Some("an example API".to_string()),
			..Info::default()
		},
		..OpenApi::default()
	};

	// Convert OpenAPI router to normal actix router, and render the doc as JSON
	let app = app.finish_api(&mut openapi);
	let openapi_rendered = Box::leak(
		serde_json::to_string(&openapi)
			.expect("Unable to render OpenAPI documentation as JSON")
			.into_boxed_str()
	);
	let app = app
		// TODO: Do we want scalar? or just swagger? redoc even?
		.route("/scalar", Scalar::new("/openapi.json").axum_route().into())
		.route(
			"/openapi.json",
			axum_get(
				async |Extension(OpenApiSpec(spec)): Extension<OpenApiSpec>| {
					([(header::CONTENT_TYPE, "application/json")], spec)
				}
			)
		)
		.layer(Extension(OpenApiSpec(openapi_rendered)));

	// Setup the listener and start the web server
	let listener = TcpListener::bind(args.bind_addr)
		.await
		.expect("Unable to bind on specififed socket address");

	axum::serve(listener, app.into_make_service())
		.await
		.unwrap()
}
