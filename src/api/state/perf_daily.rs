
use sea_orm::{ConnectionTrait as _, DatabaseConnection, DbBackend, DbErr, Statement};

pub(in crate::api) const ALL_VERSIONS: &str = "all";

pub(super) async fn rollup(database: &DatabaseConnection) -> Result<(), DbErr> {
	database
		.execute(Statement::from_sql_and_values(
			DbBackend::Postgres,
			"WITH reports AS ( \
			     SELECT (reported_at AT TIME ZONE 'UTC')::date AS day, player_id, minecraft_version, \
			            median_fps, low1_fps, low01_fps \
			     FROM perf_report \
			     WHERE reported_at >= coalesce( \
			             (SELECT max(day) - 2 FROM perf_daily)::timestamp AT TIME ZONE 'UTC', \
			             '-infinity') \
			       AND vsync IS NOT TRUE \
			       AND (max_fps IS NULL OR median_fps < max_fps * 0.9) \
			 ), per_player AS ( \
			     SELECT day, minecraft_version, \
			            percentile_cont(0.5) WITHIN GROUP (ORDER BY median_fps) AS median_fps, \
			            percentile_cont(0.5) WITHIN GROUP (ORDER BY low1_fps) AS low1_fps, \
			            percentile_cont(0.5) WITHIN GROUP (ORDER BY low01_fps) AS low01_fps \
			     FROM reports GROUP BY day, player_id, minecraft_version \
			     UNION ALL \
			     SELECT day, $1::text, \
			            percentile_cont(0.5) WITHIN GROUP (ORDER BY median_fps), \
			            percentile_cont(0.5) WITHIN GROUP (ORDER BY low1_fps), \
			            percentile_cont(0.5) WITHIN GROUP (ORDER BY low01_fps) \
			     FROM reports GROUP BY day, player_id \
			 ) \
			 INSERT INTO perf_daily (day, minecraft_version, players, median_fps, low1_fps, low01_fps) \
			 SELECT day, minecraft_version, count(*), \
			        percentile_cont(0.5) WITHIN GROUP (ORDER BY median_fps), \
			        percentile_cont(0.5) WITHIN GROUP (ORDER BY low1_fps), \
			        percentile_cont(0.5) WITHIN GROUP (ORDER BY low01_fps) \
			 FROM per_player GROUP BY day, minecraft_version \
			 ON CONFLICT (day, minecraft_version) DO UPDATE SET \
			     players = excluded.players, median_fps = excluded.median_fps, \
			     low1_fps = excluded.low1_fps, low01_fps = excluded.low01_fps",
			[ALL_VERSIONS.into()],
		))
		.await?;
	Ok(())
}
