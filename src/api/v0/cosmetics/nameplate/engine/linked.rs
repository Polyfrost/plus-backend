use std::{io::Cursor, sync::Arc};

use image::{ImageFormat, RgbaImage};
use plusrenderer::{
	container::MANIFEST,
	issues::{Issue, IssueCode, Level, Mode, path},
	nameplate::{
		LoadOptions, Loaded, NameplateAsset, Skip, Source,
		layout::reference_inputs,
		load,
		raster::{RasterOptions, raster},
	},
};

use super::{EngineError, EngineIssue, IssueLevel, NameplateEngine};

const COVER_LINES: u8 = 2;
const COVER_NAME_WIDTH: i32 = 96;
const COVER_SCALE: u8 = 4;

pub(crate) struct Core;

impl NameplateEngine for Core {
	fn validate(bundle: &[u8]) -> Vec<EngineIssue> {
		match load(Source::Zip(bundle), &LoadOptions::new(Mode::Authoring)) {
			Loaded::Asset { issues, .. }
			| Loaded::Draft { issues, .. }
			| Loaded::Invalid(issues) => issues.into_iter().map(engine_issue).collect(),
			Loaded::Skipped(skip) => vec![engine_issue(skipped(skip))],
		}
	}

	fn cover(
		bundle: &[u8],
		variant_key: Option<&str>,
	) -> Result<Option<Vec<u8>>, EngineError> {
		let asset = loaded_asset(bundle)?;
		let variant = match variant_key {
			None => 0,
			Some(key) => asset
				.variants
				.iter()
				.position(|variant| variant.key == key)
				.ok_or_else(|| {
					EngineError(format!("The bundle has no variant named {key:?}"))
				})?,
		};
		let variant = u16::try_from(variant)
			.map_err(|_| EngineError("The bundle has too many variants".to_string()))?;
		let block = reference_inputs()
			.into_iter()
			.find(|input| {
				input.line_count == COVER_LINES
					&& input.name_width == COVER_NAME_WIDTH
					&& !input.badge
			})
			.ok_or_else(|| EngineError("The engine has no cover layout".to_string()))?;
		let options = RasterOptions {
			variant,
			block,
			scale: COVER_SCALE,
			..RasterOptions::default()
		};
		let Some(drawn) = raster(&asset, &options) else {
			return Ok(None);
		};
		let image = drawn.image;
		if image.width == 0 || image.height == 0 {
			return Ok(None);
		}
		let image = RgbaImage::from_raw(image.width, image.height, image.rgba)
			.ok_or_else(|| {
				EngineError("The cover raster has the wrong size".to_string())
			})?;
		let mut png = Cursor::new(Vec::new());
		image
			.write_to(&mut png, ImageFormat::Png)
			.map_err(|error| {
				EngineError(format!("Unable to encode the cover: {error}"))
			})?;
		Ok(Some(png.into_inner()))
	}
}

fn loaded_asset(bundle: &[u8]) -> Result<Arc<NameplateAsset>, EngineError> {
	match load(Source::Zip(bundle), &LoadOptions::default()) {
		Loaded::Asset { asset, .. } => Ok(asset),
		Loaded::Draft { issues, .. } | Loaded::Invalid(issues) => Err(EngineError(
			format!("The bundle does not load: {}", describe(&issues)),
		)),
		Loaded::Skipped(skip) => Err(EngineError(format!(
			"The bundle does not load: {}",
			describe(&[skipped(skip)])
		))),
	}
}

pub(crate) fn skipped(skip: Skip) -> Issue {
	match skip {
		Skip::Format(format) => {
			Issue::error(IssueCode::FormatInvalid, path(MANIFEST, "format"))
				.value("format", format as f64)
		}
		Skip::Requires(missing) => {
			Issue::error(IssueCode::RequiresInvalid, path(MANIFEST, "requires"))
				.name("missing", &missing.join(", "))
		}
	}
}

fn describe(issues: &[Issue]) -> String {
	issues
		.iter()
		.filter(|issue| issue.level == Level::Error)
		.map(|issue| format!("{} at {}", issue.code.as_str(), issue.path))
		.collect::<Vec<_>>()
		.join("; ")
}

pub(crate) fn engine_issue(issue: Issue) -> EngineIssue {
	EngineIssue {
		code: issue.code.as_str().to_string(),
		level: match issue.level {
			Level::Error => IssueLevel::Error,
			Level::Warning => IssueLevel::Warning,
			Level::Info => IssueLevel::Info,
		},
		path: issue.path,
		values: issue.values.into_iter().collect(),
		names: issue.names.into_iter().collect(),
	}
}

#[cfg(test)]
mod tests {
	use image::{ImageFormat, load_from_memory_with_format};

	use super::Core;
	use crate::api::v0::cosmetics::{
		engine::tests::{nested, zip},
		nameplate::{IssueLevel, NameplateBundle, NameplateBundleError, NameplateEngine, validate_upload},
	};

	const KIT: &[(&str, &[u8])] = &[
		("manifest.json", include_bytes!("testdata/wardrobe_plate/manifest.json")),
		("palettes.json", include_bytes!("testdata/wardrobe_plate/palettes.json")),
		("layers/body.json", include_bytes!("testdata/wardrobe_plate/layers/body.json")),
		("layers/gem.json", include_bytes!("testdata/wardrobe_plate/layers/gem.json")),
		("layers/spark.json", include_bytes!("testdata/wardrobe_plate/layers/spark.json")),
		("sprites/gem.json", include_bytes!("testdata/wardrobe_plate/sprites/gem.json")),
		("sprites/spark.json", include_bytes!("testdata/wardrobe_plate/sprites/spark.json")),
	];

	fn broken_kit() -> Vec<u8> {
		let mut manifest: serde_json::Value = serde_json::from_slice(KIT[0].1).unwrap();
		manifest["sparkle"] = serde_json::json!(true);
		manifest["layers"].as_array_mut().unwrap().push(serde_json::json!({ "id": "missing" }));
		let manifest = serde_json::to_vec(&manifest).unwrap();
		let mut files = KIT.to_vec();
		files[0].1 = &manifest;
		zip(&files)
	}

	#[tokio::test]
	async fn the_kit_passes_and_a_broken_one_is_blocked_with_structured_errors() {
		let kit = zip(KIT);
		let bundle = NameplateBundle::read(&kit).expect("the kit reads");
		assert!(bundle.check_variant_key(Some("frost")).is_ok());
		assert_eq!(validate_upload(kit).await.expect("the kit passes"), Vec::new());

		let issues = Core::validate(&broken_kit());
		let find = |code: &str| issues.iter().find(|issue| issue.code == code).unwrap_or_else(|| panic!("{issues:?}"));
		assert_eq!((find("unknown_key").level, find("unknown_key").path.as_str()), (IssueLevel::Error, "manifest.json#sparkle"));
		assert_eq!(find("missing_file").level, IssueLevel::Error);
		assert_eq!(find("missing_file").names.get("file").map(String::as_str), Some("layers/missing.json"));
		let error = validate_upload(broken_kit()).await.expect_err("errors block the upload");
		let NameplateBundleError::Invalid(blocking) = &error else {
			panic!("expected validation issues, got {error}");
		};
		assert!(blocking.iter().all(|issue| issue.level == IssueLevel::Error));

		assert!(Core::validate(b"not a zip").iter().any(|issue| issue.level == IssueLevel::Error));
		assert!(matches!(
			NameplateBundle::read(&nested("wardrobe_plate/", KIT)),
			Err(NameplateBundleError::NestedManifest(name)) if name == "wardrobe_plate/manifest.json"
		));
	}

	#[test]
	fn covers_are_pngs_that_follow_the_variant() {
		let kit = zip(KIT);
		let png = Core::cover(&kit, None).expect("the cover renders").expect("the kit draws a cover");
		let image = load_from_memory_with_format(&png, ImageFormat::Png).expect("a PNG").to_rgba8();
		let (width, height) = image.dimensions();
		assert!((96..=2048).contains(&width) && (18..=1024).contains(&height), "{width}x{height}");
		assert!(width > height);
		let drawn = image.pixels().filter(|pixel| pixel.0[3] > 0).count();
		assert!(drawn * 4 >= (width * height) as usize, "{drawn}");

		let ember = Core::cover(&kit, Some("ember")).expect("renders");
		let frost = Core::cover(&kit, Some("frost")).expect("renders");
		assert_eq!(Some(png), ember);
		assert_ne!(ember, frost);
		assert!(Core::cover(&kit, Some("lava")).is_err());
		assert!(Core::cover(&broken_kit(), None).is_err());
	}
}
