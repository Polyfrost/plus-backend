use std::collections::HashMap;

use chrono::NaiveDate;
use entities::{analytics_client_daily, daily_playtime, player_client_info, prelude::*};
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
}

pub(super) async fn client_rows(
	txn: &DatabaseTransaction,
	day: NaiveDate,
	computed_at: DateTimeWithTimeZone,
) -> Result<Vec<analytics_client_daily::ActiveModel>, DbErr> {
	let rows = DailyPlaytime::find()
		.inner_join(PlayerClientInfo)
		.filter(daily_playtime::Column::Day.eq(day))
		.filter(daily_playtime::Column::TotalSeconds.gt(0))
		.select_only()
		.column(player_client_info::Column::ClientVersion)
		.column(player_client_info::Column::MinecraftVersion)
		.column(player_client_info::Column::Loader)
		.column(player_client_info::Column::Os)
		.into_model::<ClientRow>()
		.all(txn)
		.await?;

	let mut counts: HashMap<(String, String, String, String), i32> = HashMap::new();
	for row in rows {
		*counts
			.entry((
				row.client_version.unwrap_or_default(),
				row.minecraft_version.unwrap_or_default(),
				row.loader.unwrap_or_default(),
				row.os.unwrap_or_default(),
			))
			.or_default() += 1;
	}

	Ok(counts
		.into_iter()
		.map(
			|((client_version, minecraft_version, loader, os), players)| {
				analytics_client_daily::ActiveModel {
					day: ActiveValue::Set(day),
					client_version: ActiveValue::Set(client_version),
					minecraft_version: ActiveValue::Set(minecraft_version),
					loader: ActiveValue::Set(loader),
					os: ActiveValue::Set(os),
					active_players: ActiveValue::Set(players),
					computed_at: ActiveValue::Set(computed_at),
				}
			},
		)
		.collect())
}
