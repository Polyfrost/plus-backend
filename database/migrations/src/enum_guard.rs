use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{DbBackend, Statement};

fn quote_ident(ident: &str) -> String {
	format!("\"{}\"", ident.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
	format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn dependent_columns_query(enum_type: &str) -> String {
	format!(
		concat!(
			"SELECT n.nspname AS schema_name, c.relname AS table_name, ",
			"a.attname AS column_name, a.atttypid = t.typarray AS is_array ",
			"FROM pg_type t ",
			"JOIN pg_attribute a ON a.atttypid IN (t.oid, t.typarray) ",
			"JOIN pg_class c ON c.oid = a.attrelid ",
			"JOIN pg_namespace n ON n.oid = c.relnamespace ",
			"WHERE t.oid = to_regtype({}) AND c.relkind IN ('r', 'p') ",
			"AND a.attnum > 0 AND NOT a.attisdropped",
		),
		quote_literal(enum_type)
	)
}

pub(crate) fn usage_query(
	schema: &str,
	table: &str,
	column: &str,
	is_array: bool,
	values: &[&str],
) -> String {
	let list = values
		.iter()
		.map(|value| quote_literal(value))
		.collect::<Vec<_>>()
		.join(", ");
	let column = quote_ident(column);
	let test = if is_array {
		format!("{column}::text[] && ARRAY[{list}]::text[]")
	} else {
		format!("{column}::text IN ({list})")
	};
	format!(
		"SELECT EXISTS (SELECT 1 FROM {}.{} WHERE {test}) AS used",
		quote_ident(schema),
		quote_ident(table)
	)
}

pub(crate) async fn refuse_if_used(
	manager: &SchemaManager<'_>,
	enum_type: &str,
	values: &[&str],
) -> Result<(), DbErr> {
	let db = manager.get_connection();
	let columns = db
		.query_all(Statement::from_string(
			DbBackend::Postgres,
			dependent_columns_query(enum_type),
		))
		.await?;

	for row in columns {
		let schema: String = row.try_get("", "schema_name")?;
		let table: String = row.try_get("", "table_name")?;
		let column: String = row.try_get("", "column_name")?;
		let is_array: bool = row.try_get("", "is_array")?;
		let used = db
			.query_one(Statement::from_string(
				DbBackend::Postgres,
				usage_query(&schema, &table, &column, is_array, values),
			))
			.await?
			.map(|row| row.try_get::<bool>("", "used"))
			.transpose()?
			.unwrap_or(false);
		if used {
			return Err(DbErr::Migration(format!(
				"Can't roll back while {schema}.{table}.{column} still uses {enum_type} value(s) {}. {}",
				values.join(", "),
				"Postgres can't drop enum values, so move or delete those rows first."
			)));
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::{dependent_columns_query, usage_query};

	#[test]
	fn columns_are_found_through_the_resolved_type_in_every_schema() {
		let sql = dependent_columns_query("body_slot");
		assert!(sql.contains("to_regtype('body_slot')"), "{sql}");
		assert!(sql.contains("pg_attribute"), "{sql}");
		assert!(!sql.contains("current_schema"), "{sql}");
	}

	#[test]
	fn scalar_columns_are_checked_with_in() {
		assert_eq!(
			usage_query("public", "cosmetic", "type", false, &["effect"]),
			r#"SELECT EXISTS (SELECT 1 FROM "public"."cosmetic" WHERE "type"::text IN ('effect')) AS used"#
		);
	}

	#[test]
	fn array_columns_are_checked_for_overlap() {
		assert_eq!(
			usage_query("public", "outfit", "slots", true, &["effect_a", "effect_b"]),
			r#"SELECT EXISTS (SELECT 1 FROM "public"."outfit" WHERE "slots"::text[] && ARRAY['effect_a', 'effect_b']::text[]) AS used"#
		);
	}

	#[test]
	fn names_and_values_are_quoted() {
		assert_eq!(
			usage_query("s", r#"we"ird"#, "c", false, &["it's"]),
			r#"SELECT EXISTS (SELECT 1 FROM "s"."we""ird" WHERE "c"::text IN ('it''s')) AS used"#
		);
	}
}
