use serde::{Deserialize, Deserializer, de::DeserializeOwned, de::IntoDeserializer};

/// Parses a comma-separated query parameter into a list, deferring to `T`'s own
/// deserialization for each segment (e.g. `cape,emote` or `red,limited`).
///
/// Empty segments are ignored and an empty list deserializes to `None`, so an
/// absent and a blank parameter both mean "no filter".
pub(crate) fn deserialize_comma_list<'de, D, T>(de: D) -> Result<Option<Vec<T>>, D::Error>
where
	D: Deserializer<'de>,
	T: DeserializeOwned,
{
	use serde::de::Error;

	let Some(raw) = Option::<String>::deserialize(de)? else {
		return Ok(None);
	};

	let values = raw
		.split(',')
		.map(str::trim)
		.filter(|part| !part.is_empty())
		.map(|part| T::deserialize(part.into_deserializer()))
		.collect::<Result<Vec<T>, serde::de::value::Error>>()
		.map_err(Error::custom)?;

	Ok((!values.is_empty()).then_some(values))
}
