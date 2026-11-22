use std::{collections::HashMap, iter::repeat_with};

use chrono::NaiveDate;
use entities::{analytics_client_daily, player_client_daily, prelude::*};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, PaginatorTrait as _, QueryFilter as _, QueryOrder as _,
	QuerySelect as _, QueryTrait as _, prelude::DateTimeWithTimeZone,
};

use crate::database::active_players;

#[derive(Debug, Default, FromQueryResult)]
struct ClientRow {
	client_version: Option<String>,
	minecraft_version: Option<String>,
	loader: Option<String>,
	os: Option<String>,
	country: Option<String>,
}

pub(super) async fn client_rows(
	txn: &DatabaseTransaction,
	day: NaiveDate,
	computed_at: DateTimeWithTimeZone,
) -> Result<Vec<analytics_client_daily::ActiveModel>, DbErr> {
	let active = active_players(Some(day), Some(day));
	let active_count = active.clone().count(txn).await?;

	// What each player last reported on or before the day, not what they run now.
	let mut rows = PlayerClientDaily::find()
		.filter(player_client_daily::Column::PlayerId.in_subquery(active.into_query()))
		.filter(player_client_daily::Column::Day.lte(day))
		.distinct_on([player_client_daily::Column::PlayerId])
		.order_by_asc(player_client_daily::Column::PlayerId)
		.order_by_desc(player_client_daily::Column::Day)
		.select_only()
		.column(player_client_daily::Column::ClientVersion)
		.column(player_client_daily::Column::MinecraftVersion)
		.column(player_client_daily::Column::Loader)
		.column(player_client_daily::Column::Os)
		.column(player_client_daily::Column::Country)
		.into_model::<ClientRow>()
		.all(txn)
		.await?;

	// Players with no report land under the empty value, so every breakdown
	// adds up to the day's active players.
	let unreported = usize::try_from(active_count)
		.unwrap_or(usize::MAX)
		.saturating_sub(rows.len());
	rows.extend(repeat_with(ClientRow::default).take(unreported));

	Ok(tally(rows, day, computed_at))
}

fn tally(
	rows: Vec<ClientRow>,
	day: NaiveDate,
	computed_at: DateTimeWithTimeZone,
) -> Vec<analytics_client_daily::ActiveModel> {
	let mut counts: HashMap<(String, String, String, String, String), i32> =
		HashMap::new();
	for row in rows {
		*counts
			.entry((
				row.client_version.unwrap_or_default(),
				row.minecraft_version.unwrap_or_default(),
				row.loader.unwrap_or_default(),
				row.os.unwrap_or_default(),
				row.country.unwrap_or_default(),
			))
			.or_default() += 1;
	}

	counts
		.into_iter()
		.map(
			|((client_version, minecraft_version, loader, os, country), players)| {
				analytics_client_daily::ActiveModel {
					day: ActiveValue::Set(day),
					client_version: ActiveValue::Set(client_version),
					minecraft_version: ActiveValue::Set(minecraft_version),
					loader: ActiveValue::Set(loader),
					os: ActiveValue::Set(os),
					country: ActiveValue::Set(country),
					active_players: ActiveValue::Set(players),
					computed_at: ActiveValue::Set(computed_at),
				}
			},
		)
		.collect()
}

#[cfg(test)]
mod tests {
	use chrono::NaiveDate;
	use sea_orm::ActiveValue;

	use super::{ClientRow, tally};

	fn row(os: &str, country: Option<&str>) -> ClientRow {
		ClientRow {
			client_version: Some("1.4.2".to_owned()),
			minecraft_version: Some("1.21.4".to_owned()),
			loader: Some("fabric".to_owned()),
			os: Some(os.to_owned()),
			country: country.map(str::to_owned),
		}
	}

	#[test]
	fn splits_by_country_without_changing_client_totals() {
		let day = NaiveDate::from_ymd_opt(2026, 9, 21).expect("valid date");
		let rows = tally(
			vec![
				row("windows", Some("PL")),
				row("windows", Some("DE")),
				row("windows", Some("DE")),
				row("windows", None),
			],
			day,
			chrono::Utc::now().into(),
		);

		let mut by_country: Vec<(String, i32)> = rows
			.iter()
			.map(|model| match (&model.country, &model.active_players) {
				(ActiveValue::Set(country), ActiveValue::Set(players)) => {
					(country.clone(), *players)
				}
				_ => panic!("tally must set both country and active_players"),
			})
			.collect();
		by_country.sort();

		assert_eq!(
			by_country,
			vec![
				(String::new(), 1),
				("DE".to_owned(), 1 + 1),
				("PL".to_owned(), 1),
			],
		);
		assert_eq!(
			by_country.iter().map(|(_, players)| players).sum::<i32>(),
			4,
			"summing country away must give the count the os row had before",
		);
	}
}
