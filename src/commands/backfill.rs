use entities::{analytics_job_state, prelude::*, transaction};
use sea_orm::{
	ActiveModelTrait as _, ColumnTrait as _, Database, DatabaseConnection, DbErr,
	EntityTrait, QueryFilter as _, Set,
};
use stripe_checkout::checkout_session::RetrieveCheckoutSession;
use stripe_client::Client as StripeClient;
use tracing::{info, warn};

use crate::{api::ANALYTICS_DAILY_JOB, commands::BackfillStripeArgs};

/// Fills `amount_minor`, `currency` and `discount_minor` on transactions that
/// predate the webhook recording them, by re-reading each checkout session.
pub(crate) async fn run(args: BackfillStripeArgs) {
	let database = Database::connect(&args.database_url)
		.await
		.expect("Unable to connect to database");
	let stripe = StripeClient::new(args.stripe_secret.clone());

	let pending = Transaction::find()
		.filter(transaction::Column::StripePaymentId.is_not_null())
		.filter(transaction::Column::AmountMinor.is_null())
		.all(&database)
		.await
		.expect("Unable to query transactions");

	info!(count = pending.len(), "Backfilling stripe amounts");

	let (mut filled, mut failed) = (0usize, 0usize);
	for record in pending {
		let Some(session_id) = record.stripe_payment_id.clone() else {
			continue;
		};

		let parsed = match session_id.parse::<stripe_shared::CheckoutSessionId>() {
			Ok(parsed) => parsed,
			Err(error) => {
				warn!(%session_id, "Not a checkout session id: {error}");
				failed += 1;
				continue;
			}
		};

		let session = match RetrieveCheckoutSession::new(&parsed).send(&stripe).await {
			Ok(session) => session,
			Err(error) => {
				warn!(%session_id, "Unable to retrieve checkout session: {error}");
				failed += 1;
				continue;
			}
		};

		let mut update: transaction::ActiveModel = record.into();
		update.amount_minor = Set(session.amount_total);
		update.currency = Set(session.currency.map(|currency| currency.to_string()));
		update.discount_minor = Set(session
			.total_details
			.as_ref()
			.map(|details| details.amount_discount));

		if args.dry_run {
			update.reset_all();
			filled += 1;
			continue;
		}

		match update.update(&database).await {
			Ok(_) => filled += 1,
			Err(error) => {
				warn!(%session_id, "Unable to store amount: {error}");
				failed += 1;
			}
		}
	}

	info!(filled, failed, dry_run = args.dry_run, "Backfill finished");

	if filled > 0 && !args.dry_run {
		match reset_rollup_watermark(&database).await {
			Ok(()) => info!("Reset the analytics watermark; the rollup will recompute"),
			Err(error) => warn!(
				"Amounts were filled but the analytics watermark could not be reset, \
				 so revenue stays at its old value: {error}"
			),
		}
	}
}

async fn reset_rollup_watermark(database: &DatabaseConnection) -> Result<(), DbErr> {
	AnalyticsJobState::delete_many()
		.filter(analytics_job_state::Column::JobName.eq(ANALYTICS_DAILY_JOB))
		.exec(database)
		.await
		.map(|_| ())
}
