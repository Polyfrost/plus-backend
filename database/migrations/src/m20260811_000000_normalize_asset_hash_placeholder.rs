use sea_orm_migration::prelude::*;

/// The placeholder `m20260602_000000_cosmetics_realtime_schema` backfilled into
/// every legacy asset row: `md5("null")`.
const LEGACY_PLACEHOLDER: &str = "37a6259cc0c1dae299a7866489dff0bd";

/// The placeholder the API serves for an asset it cannot hash: `sha256("null")`,
/// mirrored by `CachedAssetInfo::DEFAULT_HASH`.
const PLACEHOLDER: &str = "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b";

/// Normalizes every unknown asset hash onto one placeholder.
///
/// `asset.hash` accumulated two ways of saying "no digest here": the legacy
/// backfill's `md5("null")` literal, and a plain null. Both now become the same
/// `sha256("null")` sentinel the API already falls back to, so the column holds
/// either a real sha256 digest or a single recognizable placeholder.
///
/// Note that a row carrying the placeholder resolves its hash straight from the
/// database and never consults object storage, so its hash is fixed until the
/// asset is uploaded again through the API — which stores the real digest.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(&format!(
				"UPDATE asset
				 SET hash = '{PLACEHOLDER}'
				 WHERE hash IS NULL OR hash = '{LEGACY_PLACEHOLDER}';"
			))
			.await
			.map(|_| ())
	}

	/// Restores the legacy placeholder so the schema round-trips, leaving real
	/// digests untouched. Rows that were null before this ran come back as the
	/// legacy literal rather than null, since the two are indistinguishable once
	/// merged.
	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(&format!(
				"UPDATE asset
				 SET hash = '{LEGACY_PLACEHOLDER}'
				 WHERE hash = '{PLACEHOLDER}';"
			))
			.await
			.map(|_| ())
	}
}
