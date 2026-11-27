#[cfg(feature = "nameplate-engine")]
mod linked;

use std::{fmt, io::Cursor};

use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::task::{JoinError, spawn_blocking};
use zip::ZipArchive;

use super::nameplate::{EngineIssue, IssueLevel, describe};
use crate::utils::zip::is_zip;

const MANIFEST: &str = "manifest.json";
const KIND_MISMATCH: &str = "kind_mismatch";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BundleKind {
	Item,
	Pet,
	Effect,
	Emote,
}

impl BundleKind {
	pub(crate) fn of(cosmetic_type: &CosmeticType) -> Option<Self> {
		match cosmetic_type {
			CosmeticType::Nameplate => None,
			CosmeticType::Pet => Some(Self::Pet),
			CosmeticType::Emote => Some(Self::Emote),
			CosmeticType::Effect => Some(Self::Effect),
			CosmeticType::Cape
			| CosmeticType::Backpack
			| CosmeticType::Glasses
			| CosmeticType::Wings
			| CosmeticType::Glove
			| CosmeticType::Hat
			| CosmeticType::Aura
			| CosmeticType::Boots
			| CosmeticType::Shoulder => Some(Self::Item),
		}
	}

	fn needs_manifest(self) -> bool {
		matches!(self, Self::Effect)
	}

	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Item => "item",
			Self::Pet => "pet",
			Self::Effect => "effect",
			Self::Emote => "emote",
		}
	}
}

impl fmt::Display for BundleKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, JsonSchema)]
pub(crate) struct BundleAnalysis {
	pub(crate) requires: Vec<String>,
	pub(crate) quads: u32,
	pub(crate) bones: u32,
	pub(crate) spring_bones: u32,
	pub(crate) live_particles: f64,
	pub(crate) trail_points: u32,
	pub(crate) record_bytes: u32,
	pub(crate) texture_bytes: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Checked {
	pub(crate) kind: Option<BundleKind>,
	pub(crate) issues: Vec<EngineIssue>,
	pub(crate) analysis: Option<BundleAnalysis>,
}

pub(crate) trait CosmeticEngine {
	fn check(bundle: &[u8], identity: u64) -> Checked;

	fn check_legacy(bundle: &[u8], identity: u64) -> Checked;
}

#[cfg_attr(feature = "nameplate-engine", allow(dead_code))]
pub(crate) struct Unlinked;

impl CosmeticEngine for Unlinked {
	fn check(_bundle: &[u8], _identity: u64) -> Checked {
		Checked::default()
	}

	fn check_legacy(_bundle: &[u8], _identity: u64) -> Checked {
		Checked::default()
	}
}

#[cfg(not(feature = "nameplate-engine"))]
pub(crate) type Engine = Unlinked;

#[cfg(feature = "nameplate-engine")]
pub(crate) type Engine = linked::Core;

#[derive(thiserror::Error, Debug)]
pub(crate) enum CosmeticBundleError {
	#[error("{0} bundles need a manifest.json at their root")]
	MissingManifest(BundleKind),
	#[error("manifest.json must sit at the root of the bundle, not at {0}")]
	NestedManifest(String),
	#[error("manifest.json has kind \"{found}\", and this cosmetic needs \"{expected}\"")]
	WrongKind {
		expected: BundleKind,
		found: BundleKind,
	},
	#[error("The bundle failed validation: {}", describe(.0))]
	Invalid(Vec<EngineIssue>),
	#[error("The bundle validator stopped: {0}")]
	ValidatorFailed(#[from] JoinError),
	#[error("Invalid ZIP bundle: {0}")]
	Zip(#[from] zip::result::ZipError),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CheckedUpload {
	pub(crate) warnings: Vec<EngineIssue>,
	pub(crate) analysis: Option<BundleAnalysis>,
}

pub(crate) fn identity(bundle: &[u8]) -> u64 {
	let digest = Sha256::digest(bundle);
	let mut prefix = [0u8; 8];
	prefix.copy_from_slice(&digest[..8]);
	u64::from_be_bytes(prefix)
}

pub(crate) fn has_root_manifest(bundle: &[u8]) -> Result<bool, CosmeticBundleError> {
	if !is_zip(bundle) {
		return Ok(false);
	}
	let archive = ZipArchive::new(Cursor::new(bundle))?;
	let mut nested = None;
	for name in (0..archive.len()).filter_map(|index| archive.name_for_index(index)) {
		if name == MANIFEST {
			return Ok(true);
		}
		if nested.is_none()
			&& let Some((_, file)) = name.rsplit_once('/')
			&& file == MANIFEST
		{
			nested = Some(name.to_string());
		}
	}
	match nested {
		Some(name) => Err(CosmeticBundleError::NestedManifest(name)),
		None => Ok(false),
	}
}

pub(crate) fn check_with<E: CosmeticEngine>(
	bundle: &[u8],
	expected: BundleKind,
) -> Result<CheckedUpload, CosmeticBundleError> {
	if !has_root_manifest(bundle)? {
		if expected.needs_manifest() {
			return Err(CosmeticBundleError::MissingManifest(expected));
		}
		if !is_zip(bundle) {
			return Ok(CheckedUpload::default());
		}
		return Ok(lenient(E::check_legacy(bundle, identity(bundle)), expected));
	}
	let checked = E::check(bundle, identity(bundle));
	if let Some(found) = checked.kind
		&& found != expected
	{
		return Err(CosmeticBundleError::WrongKind { expected, found });
	}
	let (errors, warnings): (Vec<_>, Vec<_>) = checked
		.issues
		.into_iter()
		.partition(|issue| issue.level == IssueLevel::Error);
	if !errors.is_empty() {
		return Err(CosmeticBundleError::Invalid(errors));
	}
	Ok(CheckedUpload {
		warnings,
		analysis: checked.analysis,
	})
}

fn lenient(checked: Checked, expected: BundleKind) -> CheckedUpload {
	let mut warnings: Vec<EngineIssue> = checked
		.issues
		.into_iter()
		.map(|issue| match issue.level {
			IssueLevel::Error => EngineIssue {
				level: IssueLevel::Warning,
				..issue
			},
			_ => issue,
		})
		.collect();
	if let Some(found) = checked.kind
		&& found != expected
	{
		warnings.push(kind_mismatch(expected, found));
	}
	CheckedUpload {
		warnings,
		analysis: checked.analysis,
	}
}

fn kind_mismatch(expected: BundleKind, found: BundleKind) -> EngineIssue {
	EngineIssue {
		code: KIND_MISMATCH.to_string(),
		level: IssueLevel::Warning,
		path: format!("{MANIFEST}#kind"),
		values: Default::default(),
		names: [
			("expected".to_string(), expected.to_string()),
			("found".to_string(), found.to_string()),
		]
		.into(),
	}
}

pub(crate) async fn check_upload(
	bundle: Vec<u8>,
	expected: BundleKind,
) -> Result<CheckedUpload, CosmeticBundleError> {
	spawn_blocking(move || check_with::<Engine>(&bundle, expected)).await?
}

#[cfg(test)]
pub(crate) mod tests {
	use std::io::{Cursor, Write};

	use entities::sea_orm_active_enums::CosmeticType;
	use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

	use super::{
		BundleAnalysis, BundleKind, Checked, CheckedUpload, CosmeticBundleError,
		CosmeticEngine, Unlinked, check_upload, check_with, has_root_manifest, identity,
	};
	use crate::api::v0::cosmetics::nameplate::{EngineIssue, IssueLevel};

	pub(crate) const SAMPLE: &[(&str, &[u8])] = &[
		("manifest.json", include_bytes!("engine/testdata/sample_wings/manifest.json")),
		("models/wings.geo.json", include_bytes!("engine/testdata/sample_wings/models/wings.geo.json")),
		("textures/wings.png", include_bytes!("engine/testdata/sample_wings/textures/wings.png")),
		("textures/wings.idx.png", include_bytes!("engine/testdata/sample_wings/textures/wings.idx.png")),
		("palettes.json", include_bytes!("engine/testdata/sample_wings/palettes.json")),
		(
			"animations/wings.animation.json",
			include_bytes!("engine/testdata/sample_wings/animations/wings.animation.json"),
		),
		(
			"animation_controllers/wings.animation_controllers.json",
			include_bytes!(
				"engine/testdata/sample_wings/animation_controllers/wings.animation_controllers.json"
			),
		),
	];

	pub(crate) fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
		let mut out = Cursor::new(Vec::new());
		let mut writer = ZipWriter::new(&mut out);
		let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
		for (name, data) in files {
			writer.start_file(*name, options).expect("start a test entry");
			writer.write_all(data).expect("write a test entry");
		}
		writer.finish().expect("finish the test zip");
		out.into_inner()
	}

	pub(crate) fn nested(prefix: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
		let names: Vec<String> = files.iter().map(|(name, _)| format!("{prefix}{name}")).collect();
		let files: Vec<(&str, &[u8])> =
			names.iter().zip(files).map(|(name, (_, data))| (name.as_str(), *data)).collect();
		zip(&files)
	}

	pub(crate) fn issue(code: &str, level: IssueLevel) -> EngineIssue {
		EngineIssue {
			code: code.to_string(),
			level,
			path: format!("manifest.json#{code}"),
			values: Default::default(),
			names: Default::default(),
		}
	}

	fn analysis() -> BundleAnalysis {
		BundleAnalysis {
			requires: vec!["springs".to_string()],
			quads: 12,
			bones: 3,
			spring_bones: 2,
			..BundleAnalysis::default()
		}
	}

	fn checked(kind: Option<BundleKind>, issues: &[(&str, IssueLevel)]) -> Checked {
		let issues = issues.iter().map(|(code, level)| issue(code, *level)).collect();
		let analysis = kind.as_ref().map(|_| analysis());
		Checked { kind, issues, analysis }
	}

	struct Pet;

	impl CosmeticEngine for Pet {
		fn check(_: &[u8], _: u64) -> Checked {
			checked(Some(BundleKind::Pet), &[("unreferenced_file", IssueLevel::Warning)])
		}

		fn check_legacy(_: &[u8], _: u64) -> Checked {
			panic!("bundles with a root manifest take the strict check");
		}
	}

	const STRICT: &[(&str, IssueLevel)] = &[
		("unknown_key", IssueLevel::Error),
		("cost", IssueLevel::Info),
		("missing_file", IssueLevel::Error),
	];

	struct Strict;

	impl CosmeticEngine for Strict {
		fn check(_: &[u8], _: u64) -> Checked {
			checked(None, STRICT)
		}

		fn check_legacy(_: &[u8], _: u64) -> Checked {
			checked(None, STRICT)
		}
	}

	struct LegacyPet;

	impl CosmeticEngine for LegacyPet {
		fn check(_: &[u8], _: u64) -> Checked {
			panic!("old zips take the lenient check");
		}

		fn check_legacy(_: &[u8], _: u64) -> Checked {
			checked(Some(BundleKind::Pet), &[
				("pet_legacy_invalid", IssueLevel::Error),
				("cost", IssueLevel::Info),
				("unreferenced_file", IssueLevel::Warning),
			])
		}
	}

	struct NeverCalled;

	impl CosmeticEngine for NeverCalled {
		fn check(_: &[u8], _: u64) -> Checked {
			panic!("the engine must not run without a root manifest");
		}

		fn check_legacy(_: &[u8], _: u64) -> Checked {
			panic!("the engine must not run on bundles that need a manifest");
		}
	}

	const PET_MANIFEST: &[u8] = br#"{"format":1,"kind":"pet","id":"cat"}"#;

	fn codes(issues: &[EngineIssue]) -> Vec<(&str, IssueLevel)> {
		issues.iter().map(|issue| (issue.code.as_str(), issue.level)).collect()
	}

	#[test]
	fn cosmetic_types_map_to_bundle_kinds_and_seed_the_identity() {
		assert_eq!(BundleKind::of(&CosmeticType::Pet), Some(BundleKind::Pet));
		assert_eq!(BundleKind::of(&CosmeticType::Emote), Some(BundleKind::Emote));
		assert_eq!(BundleKind::of(&CosmeticType::Nameplate), None);
		for item in [CosmeticType::Cape, CosmeticType::Wings, CosmeticType::Aura] {
			assert_eq!(BundleKind::of(&item), Some(BundleKind::Item), "{item:?}");
		}
		assert_eq!(identity(b"abc"), 0xba78_16bf_8f01_cfea);
	}

	#[test]
	fn a_root_manifest_takes_the_strict_check() {
		let pet = zip(&[("manifest.json", PET_MANIFEST)]);
		let passed = check_with::<Pet>(&pet, BundleKind::Pet).expect("warnings pass");
		let expected = CheckedUpload {
			warnings: vec![issue("unreferenced_file", IssueLevel::Warning)],
			analysis: Some(analysis()),
		};
		assert_eq!(passed, expected);

		let error = check_with::<Strict>(&pet, BundleKind::Pet).expect_err("errors block");
		let CosmeticBundleError::Invalid(issues) = &error else {
			panic!("expected validation issues, got {error}");
		};
		assert_eq!(codes(issues), [("unknown_key", IssueLevel::Error), ("missing_file", IssueLevel::Error)]);

		assert!(matches!(
			check_with::<Pet>(&pet, BundleKind::Item),
			Err(CosmeticBundleError::WrongKind { expected: BundleKind::Item, found: BundleKind::Pet })
		));
		let broken = zip(&[("manifest.json", b"{")]);
		assert_eq!(check_with::<Unlinked>(&broken, BundleKind::Item).unwrap(), CheckedUpload::default());
	}

	#[test]
	fn old_zips_only_warn() {
		let old = zip(&[("pet.json", b"{}"), ("cat.png", b"png")]);
		let checked = check_with::<LegacyPet>(&old, BundleKind::Pet).expect("old zips never fail the check");
		assert_eq!(codes(&checked.warnings), [
			("pet_legacy_invalid", IssueLevel::Warning),
			("cost", IssueLevel::Info),
			("unreferenced_file", IssueLevel::Warning),
		]);
		assert_eq!(checked.analysis, Some(analysis()));

		let checked = check_with::<Strict>(&zip(&[("wings.geo.json", b"{}")]), BundleKind::Item).unwrap();
		assert_eq!(codes(&checked.warnings), [
			("unknown_key", IssueLevel::Warning),
			("cost", IssueLevel::Info),
			("missing_file", IssueLevel::Warning),
		]);

		let checked = check_with::<LegacyPet>(&old, BundleKind::Item).expect("a kind mismatch only warns");
		let mismatch = checked.warnings.iter().find(|issue| issue.code == "kind_mismatch").unwrap();
		assert_eq!((mismatch.level, mismatch.path.as_str()), (IssueLevel::Warning, "manifest.json#kind"));
		assert_eq!(mismatch.names.get("expected").map(String::as_str), Some("item"));
		assert_eq!(mismatch.names.get("found").map(String::as_str), Some("pet"));
		assert_eq!(checked.warnings.len(), 4);
	}

	#[test]
	fn manifests_must_sit_at_the_root() {
		let nested_at = |result: Result<(), CosmeticBundleError>, at: &str| {
			matches!(result, Err(CosmeticBundleError::NestedManifest(name)) if name == at)
		};
		assert!(nested_at(check_with::<Pet>(&nested("wings/", SAMPLE), BundleKind::Pet).map(|_| ()), "wings/manifest.json"));
		assert!(nested_at(has_root_manifest(&nested("a/b/", SAMPLE)).map(|_| ()), "a/b/manifest.json"));
		for kind in [BundleKind::Item, BundleKind::Pet, BundleKind::Effect, BundleKind::Emote] {
			let result = check_with::<Unlinked>(&nested("wings/", SAMPLE), kind);
			assert!(nested_at(result.map(|_| ()), "wings/manifest.json"), "{kind}");
		}
		let both = zip(&[("wings/manifest.json", PET_MANIFEST), ("manifest.json", PET_MANIFEST)]);
		assert!(has_root_manifest(&both).unwrap());

		let loose = zip(&[("particles.json", b"{}")]);
		let nested_effect = nested("effect/", &[("manifest.json", b"{}")]);
		for result in [
			check_with::<Unlinked>(&loose, BundleKind::Effect),
			check_with::<NeverCalled>(&loose, BundleKind::Effect),
		] {
			assert!(matches!(result, Err(CosmeticBundleError::MissingManifest(BundleKind::Effect))));
		}
		for result in [
			check_with::<Unlinked>(&nested_effect, BundleKind::Effect),
			check_with::<NeverCalled>(&nested_effect, BundleKind::Effect),
		] {
			assert!(matches!(result, Err(CosmeticBundleError::NestedManifest(_))));
		}
		for kind in [BundleKind::Item, BundleKind::Pet, BundleKind::Emote] {
			assert_eq!(check_with::<Unlinked>(&loose, kind).unwrap(), CheckedUpload::default());
		}

		let png = b"\x89PNG\r\n\x1a\nrest";
		assert_eq!(check_with::<NeverCalled>(png, BundleKind::Item).unwrap(), CheckedUpload::default());
		assert!(matches!(
			check_with::<Unlinked>(b"PK\x03\x04broken", BundleKind::Item),
			Err(CosmeticBundleError::Zip(_))
		));
	}

	#[cfg(not(feature = "nameplate-engine"))]
	#[tokio::test]
	async fn uploads_pass_while_the_engine_is_unlinked() {
		let broken = zip(&[("manifest.json", br#"{"format":1,"kind":"pet"}"#)]);
		assert_eq!(check_upload(broken, BundleKind::Item).await.unwrap(), CheckedUpload::default());
		assert!(matches!(
			check_upload(nested("wings/", SAMPLE), BundleKind::Item).await,
			Err(CosmeticBundleError::NestedManifest(name)) if name == "wings/manifest.json"
		));
	}

	#[cfg(feature = "nameplate-engine")]
	#[tokio::test]
	async fn uploads_run_the_linked_engine() {
		let checked = check_upload(zip(SAMPLE), BundleKind::Item).await.expect("the sample passes");
		assert!(checked.analysis.is_some());
		assert!(matches!(
			check_upload(zip(SAMPLE), BundleKind::Pet).await,
			Err(CosmeticBundleError::WrongKind { .. })
		));
	}
}
