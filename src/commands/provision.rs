use entities::{bundles, cosmetic, cosmetic_group, prelude::*};
use sea_orm::{
	ActiveModelTrait as _, ColumnTrait as _, Database, DatabaseConnection, DbErr,
	EntityTrait, QueryFilter as _, Set,
};
use tracing::{info, warn};

use crate::{
	commands::ProvisionPaynowArgs,
	paynow::{PayNowClient, PayNowError, catalog},
	product_settings::{self, Key},
	utils::money::to_cents,
};

/// `to_cents` assumes USD; anything else misprices the whole catalogue.
const EXPECTED_CURRENCY: &str = "usd";

#[derive(Debug, Default)]
struct Totals {
	created: usize,
	adopted: usize,
	skipped: usize,
	synced: usize,
	tagged: usize,
	settings: usize,
	discounts: usize,
	failed: usize,
}

/// One sellable thing: a cosmetic group, a lone cosmetic, or a bundle.
struct Listing {
	slug: String,
	name: String,
	description: Option<String>,
	price_minor: i64,
	hidden: bool,
	/// The cosmetic rows the resulting product id is written to.
	cosmetic_ids: Vec<i32>,
	bundle_id: Option<i32>,
	existing_product_id: Option<String>,
}

pub(crate) async fn run(args: ProvisionPaynowArgs) {
	let database = Database::connect(&args.database_url)
		.await
		.expect("Unable to connect to database");
	let client = PayNowClient::new(
		&args.paynow_api_base,
		&args.paynow_store_id,
		&args.paynow_api_key,
	);

	match client.store().await {
		Ok(store) if store.currency.eq_ignore_ascii_case(EXPECTED_CURRENCY) => {}
		Ok(store) => panic!(
			"Store {} sells in {}, but prices are stored as USD major units",
			store.id, store.currency
		),
		Err(error) => panic!("Unable to read the PayNow store: {error}"),
	}

	let listings = match collect(&database).await {
		Ok(listings) => listings,
		Err(error) => panic!("Unable to read the catalogue: {error}"),
	};

	info!(count = listings.len(), "Provisioning PayNow products");

	let mut totals = Totals::default();
	for listing in listings {
		if let Err(error) =
			provision(&database, &client, &args, listing, &mut totals).await
		{
			totals.failed += 1;
			warn!("Unable to provision product: {error}");
		}
	}

	// After the products exist: a tag set can only be pushed onto a product
	// that has been provisioned.
	if args.sync_tags && !args.dry_run {
		totals.tagged = sync_tags(&database, &client, &mut totals).await;
	}
	if args.sync_settings && !args.dry_run {
		sync_settings(&database, &client, &mut totals).await;
	}
	// After tags: a discount scoped to a tag needs it on PayNow first.
	if args.sync_discounts && !args.dry_run {
		sync_discounts(&database, &client, &mut totals).await;
	}

	info!(
		created = totals.created,
		adopted = totals.adopted,
		synced = totals.synced,
		tagged = totals.tagged,
		settings = totals.settings,
		discounts = totals.discounts,
		skipped = totals.skipped,
		failed = totals.failed,
		dry_run = args.dry_run,
		"Provisioning finished"
	);
}

/// Mirrors every tag and pushes each product's set, one product at a time so
/// a single rejection does not lose the rest.
async fn sync_tags(
	database: &DatabaseConnection,
	client: &PayNowClient,
	totals: &mut Totals,
) -> usize {
	let cosmetic_ids: Vec<i32> = match Cosmetic::find()
		.filter(cosmetic::Column::StoreProductId.is_not_null())
		.all(database)
		.await
	{
		Ok(rows) => rows.into_iter().map(|row| row.id).collect(),
		Err(error) => {
			warn!("Unable to read cosmetics to tag: {error}");
			totals.failed += 1;
			return 0;
		}
	};

	info!(count = cosmetic_ids.len(), "Syncing storefront tags");

	let bundle_ids: Vec<i32> = match Bundles::find()
		.filter(bundles::Column::StoreProductId.is_not_null())
		.all(database)
		.await
	{
		Ok(rows) => rows.into_iter().map(|row| row.id).collect(),
		Err(error) => {
			warn!("Unable to read bundles to tag: {error}");
			totals.failed += 1;
			return 0;
		}
	};

	// One pass: variants share a product, so pushing per cosmetic would send
	// the same set once per variant.
	let pushed = match crate::storefront::sync_cosmetic_tags(database, client, &cosmetic_ids)
		.await
	{
		Ok(pushed) => pushed,
		Err(error) => {
			totals.failed += 1;
			// Adoption is by slug and pushing a set twice is harmless, so the
			// repair for a partial run is to run it again.
			warn!("Unable to sync tags; re-run to continue: {error}");
			0
		}
	};

	pushed
		+ match crate::storefront::sync_bundle_tags(database, client, &bundle_ids).await {
			Ok(pushed) => pushed,
			Err(error) => {
				totals.failed += 1;
				warn!("Unable to sync bundle tags; re-run to continue: {error}");
				0
			}
		}
}

async fn sync_discounts(
	database: &DatabaseConnection,
	client: &PayNowClient,
	totals: &mut Totals,
) {
	let rows = match Discount::find().all(database).await {
		Ok(rows) => rows,
		Err(error) => {
			warn!("Unable to read discounts: {error}");
			totals.failed += 1;
			return;
		}
	};
	let mut targets = match crate::pricing::targets_by_discount(database, &rows).await {
		Ok(targets) => targets,
		Err(error) => {
			warn!("Unable to read discount targets: {error}");
			totals.failed += 1;
			return;
		}
	};

	for row in rows {
		let own = targets.remove(&row.id).unwrap_or_default();
		let id = row.id;
		// Passed as its own previous version, so an existing copy is updated
		// rather than duplicated.
		let previous = row.clone();
		match crate::storefront::push_and_store(database, client, Some(&previous), row, &own)
			.await
		{
			Ok(_) => totals.discounts += 1,
			Err(error) => {
				totals.failed += 1;
				warn!(discount = id, "Unable to push discount: {error}");
			}
		}
	}
}

async fn sync_settings(
	database: &DatabaseConnection,
	client: &PayNowClient,
	totals: &mut Totals,
) {
	let rows = match ProductSettings::find().all(database).await {
		Ok(rows) => rows,
		Err(error) => {
			warn!("Unable to read product settings: {error}");
			totals.failed += 1;
			return;
		}
	};

	for row in rows {
		let Some(key) = Key::of_row(&row) else {
			continue;
		};
		let pushed = match key.product_id(database).await {
			Ok(Some(product_id)) => product_settings::push(client, &product_id, &row)
				.await
				.map_err(|error| error.to_string()),
			// Not provisioned yet; the next run with this flag picks it up.
			Ok(None) => continue,
			Err(error) => Err(error.to_string()),
		};

		match pushed {
			Ok(()) => totals.settings += 1,
			Err(error) => {
				totals.failed += 1;
				warn!(?key, "Unable to push product settings: {error}");
			}
		}
	}
}

async fn provision(
	database: &DatabaseConnection,
	client: &PayNowClient,
	args: &ProvisionPaynowArgs,
	listing: Listing,
	totals: &mut Totals,
) -> Result<(), ProvisionError> {
	if let Some(product_id) = listing.existing_product_id.clone() {
		// A group whose id only reached some of its variants, because an
		// earlier run died between the two writes.
		if !args.dry_run {
			write_back(database, &listing, &product_id).await?;
		}

		if !args.sync_prices {
			totals.skipped += 1;
			return Ok(());
		}

		info!(slug = %listing.slug, price = listing.price_minor, "Syncing price");
		if !args.dry_run {
			client
				.set_product_price(&product_id, listing.price_minor)
				.await?;
		}
		totals.synced += 1;
		return Ok(());
	}

	// Without this, a crash between creating the product and storing its id
	// leaves a duplicate behind on the next run.
	let product_id = match client.find_product_by_slug(&listing.slug).await? {
		Some(product) => {
			info!(slug = %listing.slug, "Adopting existing product");
			totals.adopted += 1;
			product.id
		}
		None => {
			info!(
				slug = %listing.slug,
				name = %listing.name,
				price = listing.price_minor,
				"Creating product"
			);
			totals.created += 1;
			if args.dry_run {
				return Ok(());
			}
			client
				.create_product(
					&listing.slug,
					&listing.name,
					listing.description.as_deref(),
					listing.price_minor,
					listing.hidden,
				)
				.await?
		}
	};

	if args.dry_run {
		return Ok(());
	}

	write_back(database, &listing, &product_id).await?;

	Ok(())
}

/// Stores the product id on every row the listing covers.
async fn write_back(
	database: &DatabaseConnection,
	listing: &Listing,
	product_id: &str,
) -> Result<(), ProvisionError> {
	if !listing.cosmetic_ids.is_empty() {
		Cosmetic::update_many()
			.col_expr(
				cosmetic::Column::StoreProductId,
				sea_orm::sea_query::Expr::value(product_id),
			)
			.filter(cosmetic::Column::Id.is_in(listing.cosmetic_ids.clone()))
			.filter(
				cosmetic::Column::StoreProductId
					.ne(product_id)
					.or(cosmetic::Column::StoreProductId.is_null()),
			)
			.exec(database)
			.await?;
	}

	if let Some(bundle_id) = listing.bundle_id
		&& let Some(bundle) = Bundles::find_by_id(bundle_id).one(database).await?
		&& bundle.store_product_id.as_deref() != Some(product_id)
	{
		let mut active: bundles::ActiveModel = bundle.into();
		active.store_product_id = Set(Some(product_id.to_string()));
		active.update(database).await?;
	}

	Ok(())
}

async fn collect(database: &DatabaseConnection) -> Result<Vec<Listing>, DbErr> {
	let mut listings = Vec::new();

	let groups: std::collections::HashMap<i32, cosmetic_group::Model> =
		CosmeticGroup::find()
			.all(database)
			.await?
			.into_iter()
			.map(|group| (group.id, group))
			.collect();

	let cosmetics = Cosmetic::find()
		.filter(cosmetic::Column::BasePrice.is_not_null())
		.all(database)
		.await?;

	// Variants share one product, so a group is one listing whose id is
	// written to every member.
	let mut by_group: std::collections::HashMap<i32, Vec<cosmetic::Model>> =
		std::collections::HashMap::new();
	for cosmetic in cosmetics {
		match cosmetic.group_id {
			Some(group_id) => by_group.entry(group_id).or_default().push(cosmetic),
			None => listings.push(Listing {
				slug: catalog::cosmetic_slug(cosmetic.id),
				name: cosmetic
					.name
					.clone()
					.unwrap_or_else(|| format!("Cosmetic #{}", cosmetic.id)),
				description: cosmetic.description.clone(),
				price_minor: to_cents(cosmetic.base_price.unwrap_or_default()),
				hidden: !cosmetic.enabled,
				existing_product_id: cosmetic.store_product_id.clone(),
				cosmetic_ids: vec![cosmetic.id],
				bundle_id: None,
			}),
		}
	}

	for (group_id, members) in by_group {
		let Some(reference) = members.first() else {
			continue;
		};
		let group = groups.get(&group_id);

		listings.push(Listing {
			slug: catalog::cosmetic_group_slug(group_id),
			name: group
				.map(|group| group.name.clone())
				.or_else(|| reference.name.clone())
				.unwrap_or_else(|| format!("Cosmetic group #{group_id}")),
			description: reference.description.clone(),
			price_minor: to_cents(reference.base_price.unwrap_or_default()),
			hidden: !group
				.map(|group| group.enabled)
				.unwrap_or(reference.enabled),
			existing_product_id: members
				.iter()
				.find_map(|member| member.store_product_id.clone()),
			cosmetic_ids: members.iter().map(|member| member.id).collect(),
			bundle_id: None,
		});
	}

	for bundle in Bundles::find()
		.filter(bundles::Column::BasePrice.is_not_null())
		.all(database)
		.await?
	{
		listings.push(Listing {
			slug: catalog::bundle_slug(bundle.id),
			name: bundle.name.clone(),
			description: bundle.description.clone(),
			price_minor: to_cents(bundle.base_price.unwrap_or_default()),
			hidden: !bundle.enabled,
			existing_product_id: bundle.store_product_id.clone(),
			cosmetic_ids: Vec::new(),
			bundle_id: Some(bundle.id),
		});
	}

	Ok(listings)
}

#[derive(Debug, thiserror::Error)]
enum ProvisionError {
	#[error("{0}")]
	PayNow(#[from] PayNowError),
	#[error("{0}")]
	Database(#[from] DbErr),
}
