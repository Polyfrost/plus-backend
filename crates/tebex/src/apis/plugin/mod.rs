use std::sync::Arc;

use crate::apis::plugin::customer_purchases::ActivePackagesRequest;

pub mod customer_purchases;

macro_rules! impl_req_funcs {
	($( $name:ident: $req:ty ),+) => {
		$(
			pub fn $name(
				&self,
				req: $req
			) -> impl Future<Output = <$req as PluginApiRequest>::Response> {
				req.fetch(self)
			}
		)+
	};
}

pub trait PluginApiRequest
where
	Self: Sized,
{
	type Response;

	fn fetch(self, client: &TebexPluginApiClient)
	-> impl Future<Output = Self::Response>;
}

#[derive(Debug, Clone)]
pub struct TebexPluginApiClient {
	inner: Arc<reqwest::Client>,
	secret: String,
}

impl TebexPluginApiClient {
	impl_req_funcs!(
		active_packages: ActivePackagesRequest<'_>
	);

	pub fn new(secret: impl Into<String>) -> Result<Self, reqwest::Error> {
		Ok(Self::new_from_client(
			secret,
			Arc::new(reqwest::Client::builder().https_only(true).build()?),
		))
	}

	pub fn new_from_client(
		secret: impl Into<String>,
		client: Arc<reqwest::Client>,
	) -> Self {
		Self {
			inner: client,
			secret: secret.into(),
		}
	}
}
