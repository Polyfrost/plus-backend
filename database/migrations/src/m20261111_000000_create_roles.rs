use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Replaces the fixed `player_role` enum with customisable roles that carry
/// 64-bit permission flags. Existing admins and moderators keep their access.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(
				r#"
				CREATE TABLE role (
					id serial PRIMARY KEY,
					name text NOT NULL UNIQUE,
					-- 0xRRGGBB, NULL for no colour
					color integer CHECK (color BETWEEN 0 AND 16777215),
					-- Higher outranks lower; the highest coloured role is the display colour
					position integer NOT NULL DEFAULT 0,
					-- u64 permission flags, stored as their two's complement bits
					permissions bigint NOT NULL DEFAULT 0,
					created_at timestamptz NOT NULL DEFAULT now()
				);

				CREATE TABLE user_role (
					user_id integer NOT NULL REFERENCES "user"(id) ON DELETE CASCADE,
					role_id integer NOT NULL REFERENCES role(id) ON DELETE CASCADE,
					PRIMARY KEY (user_id, role_id)
				);

				-- 1 = ADMINISTRATOR, 128 = VIEW_PLAYERS
				INSERT INTO role (name, position, permissions)
				VALUES ('Admin', 2, 1), ('Moderator', 1, 128);

				INSERT INTO user_role (user_id, role_id)
				SELECT u.id, r.id
				FROM "user" u
				JOIN role r ON r.name = initcap(u.role::text)
				WHERE u.role <> 'player';

				ALTER TABLE "user" DROP COLUMN role;
				DROP TYPE player_role;
				"#,
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(
				r#"
				CREATE TYPE player_role AS ENUM ('player', 'moderator', 'admin');
				ALTER TABLE "user" ADD COLUMN role player_role NOT NULL DEFAULT 'player';

				UPDATE "user" u SET role = 'moderator'
				FROM user_role ur JOIN role r ON r.id = ur.role_id
				WHERE ur.user_id = u.id AND r.permissions & 128 <> 0;

				UPDATE "user" u SET role = 'admin'
				FROM user_role ur JOIN role r ON r.id = ur.role_id
				WHERE ur.user_id = u.id AND r.permissions & 1 <> 0;

				DROP TABLE user_role;
				DROP TABLE role;
				"#,
			)
			.await?;

		Ok(())
	}
}
