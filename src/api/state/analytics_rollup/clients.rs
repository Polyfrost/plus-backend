use std::collections::HashMap;

use chrono::NaiveDate;
use entities::{
	analytics_client_daily, daily_playtime, player_client_info, player_geo, prelude::*,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, QueryFilter as _, QuerySelect as _, prelude::DateTimeWithTimeZone,
};

#[derive(Debug, FromQueryResult)]
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
	let rows = DailyPlaytime::find()
		.inner_join(PlayerClientInfo)
		.left_join(PlayerGeo)
		.filter(daily_playtime::Column::Day.eq(day))
		.filter(daily_playtime::Column::TotalSeconds.gt(0))
		.select_only()
		.column(player_client_info::Column::ClientVersion)
		.column(player_client_info::Column::MinecraftVersion)
		.column(player_client_info::Column::Loader)
		.column(player_client_info::Column::Os)
		.column(player_geo::Column::Country)
		.into_model::<ClientRow>()
		.all(txn)
		.await?;

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
