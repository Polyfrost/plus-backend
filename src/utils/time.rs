use chrono::{Datelike, NaiveDate, Utc};
use sea_orm::prelude::{Date, DateTimeWithTimeZone};

/// The first day of the current UTC month, the key monthly counters bucket by.
pub(crate) fn current_utc_month() -> Date {
	Utc::now()
		.date_naive()
		.with_day(1)
		.expect("every month has a first day")
}

/// Midnight UTC at the start of `day`.
pub(crate) fn start_of_day(day: NaiveDate) -> DateTimeWithTimeZone {
	day.and_hms_opt(0, 0, 0)
		.expect("midnight is a valid time of day")
		.and_utc()
		.fixed_offset()
}
