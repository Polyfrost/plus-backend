use plusrenderer::{
	asset::{Analysis, Kind, LoadOptions, Loaded, Source, analyze, load},
	container::{CAPABILITIES, ENGINE_CAPABILITIES},
	issues::{Issue, Mode},
};

use super::{BundleAnalysis, BundleKind, Checked, CosmeticEngine};
use crate::api::v0::cosmetics::nameplate::{EngineIssue, engine_issue, skipped};

pub(crate) struct Core;

pub(crate) fn capabilities() -> Vec<&'static str> {
	CAPABILITIES
		.iter()
		.chain(ENGINE_CAPABILITIES)
		.copied()
		.collect()
}

impl CosmeticEngine for Core {
	fn check(bundle: &[u8], identity: u64) -> Checked {
		checked_in(Mode::Authoring, bundle, identity)
	}

	fn check_legacy(bundle: &[u8], identity: u64) -> Checked {
		checked_in(Mode::Client, bundle, identity)
	}
}

fn checked_in(mode: Mode, bundle: &[u8], identity: u64) -> Checked {
	let capabilities = capabilities();
	let options = LoadOptions {
		mode,
		identity,
		capabilities: &capabilities,
	};
	match load(Source::Zip(bundle), &options) {
		Loaded::Asset { asset, issues } | Loaded::Draft { asset, issues } => Checked {
			kind: bundle_kind(asset.kind),
			issues: engine_issues(issues),
			analysis: Some(bundle_analysis(analyze(&asset))),
		},
		Loaded::Invalid(issues) => Checked {
			issues: engine_issues(issues),
			..Checked::default()
		},
		Loaded::Skipped(skip) => Checked {
			issues: vec![engine_issue(skipped(skip))],
			..Checked::default()
		},
		Loaded::Legacy(_) => Checked::default(),
	}
}

fn bundle_kind(kind: Kind) -> Option<BundleKind> {
	match kind {
		Kind::Item => Some(BundleKind::Item),
		Kind::Pet => Some(BundleKind::Pet),
		Kind::Effect => Some(BundleKind::Effect),
		Kind::Emote => Some(BundleKind::Emote),
		Kind::Nameplate => None,
	}
}

fn engine_issues(issues: Vec<Issue>) -> Vec<EngineIssue> {
	issues.into_iter().map(engine_issue).collect()
}

fn bundle_analysis(analysis: Analysis) -> BundleAnalysis {
	BundleAnalysis {
		requires: analysis.requires,
		quads: analysis.quads,
		bones: analysis.bones,
		spring_bones: analysis.spring_bones,
		live_particles: analysis.live_particles,
		trail_points: analysis.trail_points,
		record_bytes: analysis.record_bytes,
		texture_bytes: analysis.texture_bytes,
	}
}

#[cfg(test)]
mod tests {
	use plusrenderer::{
		asset::{LoadOptions, Loaded, Source, load},
		issues::Mode,
	};
	use serde_json::{Value, json};

	use super::{Core, capabilities};
	use crate::api::v0::cosmetics::{
		color_slots::customizable_slots,
		engine::{
			BundleKind, CosmeticBundleError, CosmeticEngine, check_with,
			tests::{SAMPLE, nested, zip},
		},
		nameplate::{EngineIssue, IssueLevel},
	};

	fn sample_with(edit: impl FnOnce(&mut Value)) -> Vec<u8> {
		let mut manifest: Value = serde_json::from_slice(SAMPLE[0].1).unwrap();
		edit(&mut manifest);
		let manifest = serde_json::to_vec(&manifest).unwrap();
		let mut files = SAMPLE.to_vec();
		files[0].1 = &manifest;
		zip(&files)
	}

	fn file(name: &str) -> &'static [u8] {
		SAMPLE.iter().find(|(file, _)| *file == name).map(|(_, data)| *data).unwrap()
	}

	fn issues(bundle: &[u8], kind: BundleKind) -> Vec<EngineIssue> {
		match check_with::<Core>(bundle, kind) {
			Err(CosmeticBundleError::Invalid(issues)) => issues,
			other => panic!("expected validation issues, got {other:?}"),
		}
	}

	#[test]
	fn the_sample_item_passes_with_its_analysis() {
		let checked = Core::check(&zip(SAMPLE), 42);
		assert_eq!((checked.kind, checked.issues), (Some(BundleKind::Item), Vec::new()));
		let analysis = checked.analysis.expect("an analysis");
		assert_eq!(analysis.requires, ["controllers", "reactions", "palettes", "custom_colors"]);
		assert_eq!((analysis.quads, analysis.bones), (6, 2));
		assert_eq!(analysis.texture_bytes, 16 * 16 * 5);
		assert!(analysis.record_bytes > 0);
		let value = serde_json::to_value(&analysis).unwrap();
		assert_eq!(value["requires"], json!(["controllers", "reactions", "palettes", "custom_colors"]));
		assert_eq!((&value["quads"], &value["spring_bones"]), (&json!(6), &json!(0)));

		let upload = check_with::<Core>(&zip(SAMPLE), BundleKind::Item).expect("the sample uploads");
		assert_eq!((upload.warnings, upload.analysis), (Vec::new(), Some(analysis)));
		assert!(matches!(
			check_with::<Core>(&zip(SAMPLE), BundleKind::Pet),
			Err(CosmeticBundleError::WrongKind { expected: BundleKind::Pet, found: BundleKind::Item })
		));
		assert!(matches!(
			check_with::<Core>(&nested("sample_wings/", SAMPLE), BundleKind::Item),
			Err(CosmeticBundleError::NestedManifest(name)) if name == "sample_wings/manifest.json"
		));
	}

	#[test]
	fn manifest_problems_block_the_upload() {
		let found = issues(
			&sample_with(|manifest| {
				manifest["colourways"] = json!({});
				manifest["animations"] = json!(["wings", "missing"]);
			}),
			BundleKind::Item,
		);
		let at = |code: &str| found.iter().find(|issue| issue.code == code).map(|issue| issue.path.as_str());
		assert_eq!(at("unknown_key"), Some("manifest.json#colourways"));
		assert!(at("missing_file").is_some(), "{found:?}");

		let beyond = sample_with(|manifest| manifest["requires"].as_array_mut().unwrap().push(json!("banners")));
		let found = issues(&beyond, BundleKind::Item);
		assert_eq!(found.len(), 1, "{found:?}");
		assert_eq!((found[0].code.as_str(), found[0].path.as_str()), ("requires_invalid", "manifest.json#requires"));
		assert_eq!(found[0].names.get("missing").map(String::as_str), Some("banners"));

		let codes = |bundle: &[u8]| -> Vec<(String, String)> {
			issues(bundle, BundleKind::Item).into_iter().map(|issue| (issue.code, issue.path)).collect()
		};
		let plate = zip(&[("manifest.json", br#"{"format":1,"kind":"nameplate","id":"plate"}"#)]);
		assert_eq!(codes(&plate), [("asset_kind_unsupported".to_owned(), "manifest.json#kind".to_owned())]);
	}

	#[test]
	fn old_zips_keep_loading_with_at_most_warnings() {
		let old = zip(&[
			("wings.geo.json", file("models/wings.geo.json")),
			("wings.png", file("textures/wings.png")),
			("wings.animation.json", file("animations/wings.animation.json")),
		]);
		let upload = check_with::<Core>(&old, BundleKind::Item).expect("old zips pass");
		assert_eq!((upload.analysis, upload.warnings), (None, Vec::new()));

		let pet = zip(&[("pet.json", b"{}"), ("cat.png", file("textures/wings.png"))]);
		let upload = check_with::<Core>(&pet, BundleKind::Pet).expect("old zips never fail the check");
		assert_eq!(upload.analysis, None);
		let codes: Vec<_> = upload.warnings.iter().map(|issue| (issue.code.as_str(), issue.level)).collect();
		assert_eq!(codes, [("pet_legacy_invalid", IssueLevel::Warning)]);

		let emote = zip(&[("wave.animation.json", file("animations/wings.animation.json"))]);
		let upload = check_with::<Core>(&emote, BundleKind::Emote).expect("the old emote uploads");
		assert!(upload.analysis.is_some());
		let clean = |issue: &EngineIssue| issue.level != IssueLevel::Error && issue.code != "kind_mismatch";
		assert!(upload.warnings.iter().all(clean), "{:?}", upload.warnings);
		let as_item = check_with::<Core>(&emote, BundleKind::Item).expect("a mismatch only warns");
		let mismatch = as_item.warnings.iter().find(|issue| issue.code == "kind_mismatch").unwrap();
		assert_eq!(mismatch.names.get("found").map(String::as_str), Some("emote"));
		assert_eq!(mismatch.names.get("expected").map(String::as_str), Some("item"));
	}

	#[test]
	fn customizable_slots_match_the_loaded_palette() {
		let data = zip(SAMPLE);
		let capabilities = capabilities();
		let options = LoadOptions { mode: Mode::Client, identity: 1, capabilities: &capabilities };
		let Loaded::Asset { asset, .. } = load(Source::Zip(&data), &options) else {
			panic!("the sample loads");
		};
		let slots = asset.palette.slots.iter().filter(|slot| slot.customizable);
		let loaded: Vec<String> = slots.map(|slot| slot.id.clone()).collect();
		assert_eq!(customizable_slots(&data), loaded);
		assert_eq!(loaded, ["feather"]);
	}
}
