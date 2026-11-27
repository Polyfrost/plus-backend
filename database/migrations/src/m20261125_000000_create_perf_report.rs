use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PerfReport {
	Table,
	Id,
	PlayerId,
	ReportedAt,
	ClientVersion,
	MinecraftVersion,
	Loader,
	Os,
	OsVersion,
	JavaVersion,
	Cpu,
	CpuCores,
	RamMb,
	HeapMb,
	Gpu,
	RenderDistance,
	MaxFps,
	Vsync,
	Shaders,
	ModCount,
	PackHash,
	Seconds,
	Frames,
	AvgFps,
	MedianFps,
	Low1Fps,
	Low01Fps,
}

#[derive(DeriveIden)]
enum PerfDaily {
	Table,
	Day,
	MinecraftVersion,
	Players,
	MedianFps,
	Low1Fps,
	Low01Fps,
}

#[derive(DeriveIden)]
enum PerfPack {
	Table,
	PackHash,
	Mods,
	FirstSeenAt,
}

#[derive(DeriveIden)]
enum PerfRegression {
	Table,
	MinecraftVersion,
	Loader,
	ModId,
	FromVersion,
	ToVersion,
	Metric,
	Pairs,
	Ratio,
	SoloPairs,
	SoloRatio,
	DetectedAt,
	UpdatedAt,
	Notified,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PerfReport::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PerfReport::Id)
							.big_integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(PerfReport::PlayerId).integer().not_null())
					.col(
						ColumnDef::new(PerfReport::ReportedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(ColumnDef::new(PerfReport::ClientVersion).text().not_null())
					.col(ColumnDef::new(PerfReport::MinecraftVersion).text().not_null())
					.col(ColumnDef::new(PerfReport::Loader).text().not_null())
					.col(ColumnDef::new(PerfReport::Os).text().not_null())
					.col(ColumnDef::new(PerfReport::OsVersion).text().null())
					.col(ColumnDef::new(PerfReport::JavaVersion).text().null())
					.col(ColumnDef::new(PerfReport::Cpu).text().null())
					.col(ColumnDef::new(PerfReport::CpuCores).integer().not_null())
					.col(ColumnDef::new(PerfReport::RamMb).big_integer().null())
					.col(ColumnDef::new(PerfReport::HeapMb).big_integer().not_null())
					.col(ColumnDef::new(PerfReport::Gpu).text().null())
					.col(ColumnDef::new(PerfReport::RenderDistance).integer().null())
					.col(ColumnDef::new(PerfReport::MaxFps).integer().null())
					.col(ColumnDef::new(PerfReport::Vsync).boolean().null())
					.col(ColumnDef::new(PerfReport::Shaders).boolean().null())
					.col(ColumnDef::new(PerfReport::ModCount).integer().not_null())
					.col(ColumnDef::new(PerfReport::PackHash).text().not_null())
					.col(ColumnDef::new(PerfReport::Seconds).double().not_null())
					.col(ColumnDef::new(PerfReport::Frames).integer().not_null())
					.col(ColumnDef::new(PerfReport::AvgFps).double().not_null())
					.col(ColumnDef::new(PerfReport::MedianFps).double().not_null())
					.col(ColumnDef::new(PerfReport::Low1Fps).double().not_null())
					.col(ColumnDef::new(PerfReport::Low01Fps).double().not_null())
					.foreign_key(
						ForeignKey::create()
							.from(PerfReport::Table, PerfReport::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_perf_report_player_reported_at")
					.table(PerfReport::Table)
					.col(PerfReport::PlayerId)
					.col(PerfReport::ReportedAt)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_perf_report_cohort")
					.table(PerfReport::Table)
					.col(PerfReport::MinecraftVersion)
					.col(PerfReport::Loader)
					.col(PerfReport::ReportedAt)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_perf_report_reported_at")
					.table(PerfReport::Table)
					.col(PerfReport::ReportedAt)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_perf_report_pack_hash")
					.table(PerfReport::Table)
					.col(PerfReport::PackHash)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PerfDaily::Table)
					.if_not_exists()
					.col(ColumnDef::new(PerfDaily::Day).date().not_null())
					.col(ColumnDef::new(PerfDaily::MinecraftVersion).text().not_null())
					.col(ColumnDef::new(PerfDaily::Players).integer().not_null())
					.col(ColumnDef::new(PerfDaily::MedianFps).double().not_null())
					.col(ColumnDef::new(PerfDaily::Low1Fps).double().not_null())
					.col(ColumnDef::new(PerfDaily::Low01Fps).double().not_null())
					.primary_key(
						Index::create()
							.col(PerfDaily::Day)
							.col(PerfDaily::MinecraftVersion),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PerfPack::Table)
					.if_not_exists()
					.col(ColumnDef::new(PerfPack::PackHash).text().not_null().primary_key())
					.col(ColumnDef::new(PerfPack::Mods).json_binary().not_null())
					.col(
						ColumnDef::new(PerfPack::FirstSeenAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PerfRegression::Table)
					.if_not_exists()
					.col(ColumnDef::new(PerfRegression::MinecraftVersion).text().not_null())
					.col(ColumnDef::new(PerfRegression::Loader).text().not_null())
					.col(ColumnDef::new(PerfRegression::ModId).text().not_null())
					.col(ColumnDef::new(PerfRegression::FromVersion).text().not_null())
					.col(ColumnDef::new(PerfRegression::ToVersion).text().not_null())
					.col(ColumnDef::new(PerfRegression::Metric).text().not_null())
					.col(ColumnDef::new(PerfRegression::Pairs).integer().not_null())
					.col(ColumnDef::new(PerfRegression::Ratio).double().not_null())
					.col(ColumnDef::new(PerfRegression::SoloPairs).integer().not_null())
					.col(ColumnDef::new(PerfRegression::SoloRatio).double().null())
					.col(
						ColumnDef::new(PerfRegression::DetectedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PerfRegression::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PerfRegression::Notified)
							.boolean()
							.not_null()
							.default(false),
					)
					.primary_key(
						Index::create()
							.col(PerfRegression::MinecraftVersion)
							.col(PerfRegression::Loader)
							.col(PerfRegression::ModId)
							.col(PerfRegression::FromVersion)
							.col(PerfRegression::ToVersion)
							.col(PerfRegression::Metric),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PerfRegression::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PerfPack::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PerfDaily::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PerfReport::Table).to_owned())
			.await
	}
}
