use std::{
	fmt::{self, Display, Formatter},
	str::FromStr,
};

use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Select};
use serde::{Deserialize, Serialize};
use serde_with::DeserializeFromStr;

/// The largest page a client may ask for, whatever `nb` it sends.
pub(crate) const MAX_PAGE_SIZE: u64 = 100;

/// Serde default for the page size (`nb`).
pub(crate) fn default_page_size() -> u64 {
	50
}

/// Serde default for the 1-indexed page number.
pub(crate) fn default_page() -> u64 {
	1
}

/// Where the previous page ended, as `<rank>.<id>`, both sorted descending.
/// Lists sorted by id alone use the id as the rank.
#[derive(Debug, Clone, Copy, DeserializeFromStr)]
pub(crate) struct Cursor {
	pub(crate) rank: i64,
	pub(crate) id: i32,
}

impl Cursor {
	pub(crate) fn id(id: i32) -> Self {
		Self {
			rank: id.into(),
			id,
		}
	}
}

impl Display for Cursor {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{}.{}", self.rank, self.id)
	}
}

impl FromStr for Cursor {
	type Err = &'static str;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let (rank, id) = s.split_once('.').ok_or("invalid cursor")?;
		Ok(Self {
			rank: rank.parse().map_err(|_| "invalid cursor")?,
			id: id.parse().map_err(|_| "invalid cursor")?,
		})
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CursorQuery {
	/// The `cursor` returned with the previous page.
	#[schemars(with = "Option<String>")]
	cursor: Option<Cursor>,
	/// Defaults to 50, at most 100.
	limit: Option<u64>,
}

/// The slice of a list to load.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Page {
	pub(crate) cursor: Option<Cursor>,
	limit: u64,
}

impl Page {
	/// The first `limit` items, for the v0 lists that predate paging.
	pub(crate) fn first(limit: u64) -> Self {
		Self {
			cursor: None,
			limit,
		}
	}

	/// One past the limit, so a leftover row means there is another page.
	pub(crate) fn fetch_limit(self) -> u64 {
		self.limit + 1
	}
}

impl From<CursorQuery> for Page {
	fn from(query: CursorQuery) -> Self {
		Self {
			cursor: query.cursor,
			limit: query
				.limit
				.unwrap_or(default_page_size())
				.clamp(1, MAX_PAGE_SIZE),
		}
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CursorPage<T> {
	pub(crate) items: Vec<T>,
	/// Pass back as `cursor` for the next page; absent on the last page.
	pub(crate) cursor: Option<String>,
}

/// Trims rows fetched with [`Page::fetch_limit`].
pub(crate) fn split_page<R>(
	mut rows: Vec<R>,
	page: Page,
	cursor_of: impl Fn(&R) -> Cursor,
) -> (Vec<R>, Option<String>) {
	let limit = usize::try_from(page.limit).unwrap_or(usize::MAX);
	if rows.len() <= limit {
		return (rows, None);
	}
	rows.truncate(limit);
	let cursor = rows.last().map(|row| cursor_of(row).to_string());
	(rows, cursor)
}

/// Newest first by `id`, resuming after the cursor.
pub(crate) fn keyset_by_id<E: EntityTrait>(
	select: Select<E>,
	id: E::Column,
	page: Page,
) -> Select<E> {
	let select = select.order_by_desc(id).limit(page.fetch_limit());
	match page.cursor {
		Some(cursor) => select.filter(id.lt(cursor.id)),
		None => select,
	}
}

#[cfg(test)]
mod tests {
	use super::{Cursor, CursorQuery, Page, split_page};

	#[test]
	fn pages_split_on_the_extra_row() {
		let page = Page::first(2);
		let (rows, cursor) = split_page(vec![9, 8, 7], page, |&id| Cursor::id(id));
		assert_eq!(rows, [9, 8]);
		assert_eq!(cursor.as_deref(), Some("8.8"));

		let (rows, cursor) = split_page(vec![9, 8], page, |&id| Cursor::id(id));
		assert_eq!(rows, [9, 8]);
		assert!(cursor.is_none());

		// Clients can't ask past the page size; v0 caps can.
		let asked: CursorQuery =
			serde_json::from_str(r#"{"limit": 1000}"#).expect("the query is valid");
		assert_eq!(Page::from(asked).limit, 100);

		let parsed: Cursor = "-3.4".parse().expect("the cursor is valid");
		assert_eq!((parsed.rank, parsed.id), (-3, 4));
		assert!("4".parse::<Cursor>().is_err());
	}
}
