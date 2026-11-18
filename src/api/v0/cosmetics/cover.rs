use std::{
	f32::consts::PI,
	io::{self, Cursor, Read},
	str::{Utf8Error, from_utf8},
};

use entities::sea_orm_active_enums::{BodySlot, CosmeticType};
use image::{ImageError, ImageFormat, RgbaImage, load_from_memory_with_format};
use plusrenderer::{
	Affine, Arms, BodySlot as RenderSlot, Camera, Scene, animation, bedrock, build::Buffers,
	bundle, mesh::Mesh, molang::Interner,
};
use reqwest::Client;
use sea_orm::ActiveEnum;
use tokio::{
	sync::OnceCell,
	task::{JoinError, spawn_blocking},
};
use webp_animation::{Encoder, Error as WebpError};
use zip::{ZipArchive, result::ZipError};

use crate::utils::zip::is_zip;

const WIDTH: u32 = 400;
const HEIGHT: u32 = 600;
const PLAYER: usize = 0;
const COSMETIC: usize = 1;
/// The skinview3d renderer's camera, which covers keep matching. Yaw in radians.
const YAW: f32 = 0.4;
const FOV_Y: f32 = 50.0;
const ZOOM: f32 = 0.9;
const RENDERER_FOV_Y: f32 = 45.0;
/// skinview3d's player parts in model space, outer layer included.
/// Its hidden ears still counted towards the head.
const HEAD: [[[f32; 3]; 2]; 3] = [
	[[-4.5, 23.5, -4.5], [4.5, 32.5, 4.5]],
	[[-10.0, 30.0, 0.0], [-2.0, 38.0, 4.0 / 3.0]],
	[[2.0, 30.0, 0.0], [10.0, 38.0, 4.0 / 3.0]],
];
const BODY: [[[f32; 3]; 2]; 1] = [[[-4.25, 11.75, -2.25], [4.25, 24.25, 2.25]]];
/// Default time animations and emotes are captured at, rather than at rest.
const POSE_SECONDS: f32 = 1.0;
/// The game's tick rate, which animations are authored against.
const PREVIEW_FPS: u32 = 20;
const MAX_PREVIEW_SECONDS: f32 = 8.0;
pub(super) const PREVIEW_CONTENT_TYPE: &str = "image/webp";
pub(super) const PREVIEW_EXTENSION: &str = "webp";
/// Classic "Steve" skin on Mojang's texture server.
const DEFAULT_SKIN_URL: &str = "https://textures.minecraft.net/texture/31f477eb1a7beee631c2ca64d06f8f68fa93a3386d04452ab27f43acdf1b60cb";

static DEFAULT_SKIN: OnceCell<RgbaImage> = OnceCell::const_new();

/// `[min_x, min_y, max_x, max_y]` in skinview3d's world.
type Bounds = [f32; 4];

#[derive(Clone, Copy)]
enum Region {
	Head,
	Body,
	Cosmetic,
}

enum Center {
	Anchor(&'static [Region]),
	/// A share of the fitted region's horizontal centre.
	FitX(f32),
}

struct Focus {
	center: Center,
	fit: &'static [Region],
	margin: f32,
}

#[derive(thiserror::Error, Debug)]
pub(super) enum RenderError {
	#[error("Unable to fetch the default skin: {0}")]
	Skin(#[from] reqwest::Error),
	#[error("Unable to read bundle: {0}")]
	Zip(#[from] ZipError),
	#[error("Unable to read bundle: {0}")]
	Io(#[from] io::Error),
	#[error("Bundle file is not UTF-8: {0}")]
	Utf8(#[from] Utf8Error),
	#[error("Image error: {0}")]
	Image(#[from] ImageError),
	#[error("Unable to encode preview: {0}")]
	Webp(#[from] WebpError),
	#[error("Invalid geometry: {0}")]
	Geometry(#[from] bedrock::Error),
	#[error("Invalid animation: {0}")]
	Animation(#[from] animation::Error),
	#[error("Bundle has no {0}")]
	Missing(&'static str),
	#[error("Unknown body slot {0}")]
	Slot(String),
	#[error("Render task failed: {0}")]
	Join(#[from] JoinError),
}

pub(super) struct Renders {
	/// PNG still.
	pub(super) cover: Vec<u8>,
	/// Looping lossless animated WebP, for cosmetics that move.
	pub(super) preview: Option<Vec<u8>>,
}

/// Renders the cosmetic worn by the default player: a cover posed `seconds`
/// into its animation or emote, and a preview of one loop when it moves.
#[tracing::instrument(level = "debug", skip(client, data), fields(bytes = data.len()))]
pub(super) async fn render_cosmetic(
	client: &Client,
	cosmetic_type: CosmeticType,
	slots: &[BodySlot],
	model_variant: Option<&str>,
	seconds: Option<f32>,
	data: Vec<u8>,
) -> Result<Renders, RenderError> {
	let skin = DEFAULT_SKIN
		.get_or_try_init(|| fetch_skin(client))
		.await?
		.clone();
	let arms = if model_variant == Some("slim") {
		Arms::Slim
	} else {
		Arms::Wide
	};
	let slot = slots.first().cloned();
	let seconds = seconds
		.filter(|s| s.is_finite() && *s >= 0.0)
		.unwrap_or(POSE_SECONDS);

	spawn_blocking(move || draw(skin, arms, cosmetic_type, slot, seconds, &data)).await?
}

async fn fetch_skin(client: &Client) -> Result<RgbaImage, RenderError> {
	let bytes = client
		.get(DEFAULT_SKIN_URL)
		.send()
		.await?
		.error_for_status()?
		.bytes()
		.await?;
	decode(&bytes)
}

struct Loaded {
	scene: Scene,
	textures: Vec<Option<RgbaImage>>,
	/// Seconds in one loop, when anything animates.
	length: Option<f32>,
}

fn draw(
	skin: RgbaImage,
	arms: Arms,
	cosmetic_type: CosmeticType,
	slot: Option<BodySlot>,
	seconds: f32,
	data: &[u8],
) -> Result<Renders, RenderError> {
	let Loaded {
		mut scene,
		textures,
		length,
	} = load(skin, arms, &cosmetic_type, slot, data)?;

	scene.pose(seconds);
	let camera = camera(&mut scene, &cosmetic_type);

	let mut cover = Cursor::new(Vec::new());
	frames(&mut scene, &camera, &textures, &[seconds])
		.remove(0)
		.write_to(&mut cover, ImageFormat::Png)?;

	let preview = match length.filter(|l| *l > 0.0) {
		Some(length) => {
			let count = (length.min(MAX_PREVIEW_SECONDS) * PREVIEW_FPS as f32).ceil() as u32;
			let times: Vec<f32> = (0..count.max(2))
				.map(|i| i as f32 / PREVIEW_FPS as f32)
				.collect();
			Some(encode_webp(frames(&mut scene, &camera, &textures, &times))?)
		}
		None => None,
	};

	Ok(Renders {
		cover: cover.into_inner(),
		preview,
	})
}

fn load(
	skin: RgbaImage,
	arms: Arms,
	cosmetic_type: &CosmeticType,
	slot: Option<BodySlot>,
	data: &[u8],
) -> Result<Loaded, RenderError> {
	let files = if is_zip(data) {
		unzip(data)?
	} else {
		vec![("cosmetic.png".to_owned(), data.to_vec())]
	};
	let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
	let parts = bundle::parts(&names);
	let file = |index: Option<usize>, what| {
		index
			.map(|i| files[i].1.as_slice())
			.ok_or(RenderError::Missing(what))
	};

	let mut scene = Scene::default();
	scene.load_player(PLAYER, arms);
	let mut textures = vec![Some(skin), None];
	let mut length = None;

	match cosmetic_type {
		CosmeticType::Cape => {
			let still = decode(file(parts.texture, "texture")?)?;
			let sheet = match parts.sheet {
				Some(index) => {
					let sheet = decode(&files[index].1)?;
					let (frames, frame_seconds) = bundle::cape_sheet(
						still.dimensions().into(),
						sheet.dimensions().into(),
						&files[index].0,
					);
					(frames > 1).then_some((sheet, frames, frame_seconds))
				}
				None => None,
			};

			match sheet {
				Some((sheet, frames, frame_seconds)) => {
					scene.load_cape(COSMETIC, frames, frame_seconds);
					scene.play(COSMETIC, 0.0);
					textures[COSMETIC] = Some(sheet);
					length = Some(frames as f32 * frame_seconds);
				}
				None => {
					scene.load_cape(COSMETIC, 1, 1.0);
					textures[COSMETIC] = Some(still);
				}
			}
		}
		CosmeticType::Emote => {
			let json = from_utf8(file(parts.animation, "animation")?)?;
			scene.load_animation(PLAYER, json)?;
			scene.play(PLAYER, 0.0);
			length = animation_length(json)?;
		}
		_ => {
			let texture = decode(file(parts.texture, "texture")?)?;
			let model = bedrock::parse_one(from_utf8(file(parts.geometry, "geometry")?)?)?;
			scene.load_cosmetic(
				COSMETIC,
				model,
				body_slot(cosmetic_type, slot)?,
				None,
				Some(texture.dimensions().into()),
			);
			textures[COSMETIC] = Some(texture);

			if let Some(index) = parts.animation {
				let json = from_utf8(&files[index].1)?;
				scene.load_animation(COSMETIC, json)?;
				scene.play(COSMETIC, 0.0);
				length = animation_length(json)?;
			}
		}
	}

	Ok(Loaded {
		scene,
		textures,
		length,
	})
}

/// The scene only plays a file's first animation.
fn animation_length(json: &str) -> Result<Option<f32>, RenderError> {
	Ok(animation::parse(json, &mut Interner::default())?
		.first()
		.map(|animation| animation.length))
}

fn decode(bytes: &[u8]) -> Result<RgbaImage, RenderError> {
	Ok(load_from_memory_with_format(bytes, ImageFormat::Png)?.to_rgba8())
}

fn unzip(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, RenderError> {
	let mut archive = ZipArchive::new(Cursor::new(data))?;
	(0..archive.len())
		.map(|i| {
			let mut entry = archive.by_index(i)?;
			let mut bytes = Vec::new();
			entry.read_to_end(&mut bytes)?;
			Ok((entry.name().to_owned(), bytes))
		})
		.collect()
}

/// The cosmetic's first slot, else the one its type implies.
fn body_slot(
	cosmetic_type: &CosmeticType,
	slot: Option<BodySlot>,
) -> Result<RenderSlot, RenderError> {
	let name = match (slot, cosmetic_type) {
		(Some(slot), _) => slot.to_value(),
		(None, CosmeticType::Glove) => "right_hand".to_owned(),
		(None, other) => other.to_value(),
	};
	RenderSlot::from_name(&name).ok_or(RenderError::Slot(name))
}

/// Lossless and looping forever, the encoder's defaults.
fn encode_webp(frames: Vec<RgbaImage>) -> Result<Vec<u8>, RenderError> {
	let frame_ms = 1000 / PREVIEW_FPS as i32;
	let mut encoder = Encoder::new((WIDTH, HEIGHT))?;
	let mut timestamp = 0;
	for frame in &frames {
		encoder.add_frame(frame.as_raw(), timestamp)?;
		timestamp += frame_ms;
	}
	Ok(encoder.finalize(timestamp)?.to_vec())
}

/// skinview3d's per-type framing. Others keep the default zoom on the player's middle.
fn focus(cosmetic_type: &CosmeticType) -> Option<Focus> {
	Some(match cosmetic_type {
		// Mounted on the head, so skinview3d counted them as part of it.
		CosmeticType::Hat | CosmeticType::Glasses => Focus {
			center: Center::Anchor(&[Region::Head, Region::Cosmetic]),
			fit: &[Region::Head, Region::Cosmetic],
			margin: 1.3,
		},
		CosmeticType::Boots => Focus {
			center: Center::Anchor(&[Region::Cosmetic]),
			fit: &[Region::Cosmetic],
			margin: 1.6,
		},
		CosmeticType::Shoulder => Focus {
			center: Center::Anchor(&[Region::Cosmetic]),
			fit: &[Region::Cosmetic, Region::Head],
			margin: 1.35,
		},
		CosmeticType::Wings => Focus {
			center: Center::FitX(0.25),
			fit: &[Region::Cosmetic, Region::Body],
			margin: 1.6,
		},
		CosmeticType::Backpack => Focus {
			center: Center::Anchor(&[Region::Cosmetic]),
			fit: &[Region::Cosmetic],
			margin: 1.25,
		},
		_ => return None,
	})
}

/// Level, looking at the player's middle, with the player turned and the posed scene framed.
fn camera(scene: &mut Scene, cosmetic_type: &CosmeticType) -> Camera {
	// Back-worn cosmetics are hidden from the front.
	let yaw = match cosmetic_type {
		CosmeticType::Cape | CosmeticType::Wings | CosmeticType::Backpack => PI + YAW,
		_ => YAW,
	};
	let world = Affine::rot_y(yaw).mul(&Affine::translation([0.0, -16.0, 0.0]));
	let (zoom, center) = focus(cosmetic_type)
		.and_then(|focus| frame(scene, &world, &focus))
		.unwrap_or((ZOOM, [0.0; 2]));
	let distance = (4.5 + 16.5 / (FOV_Y / 2.0).to_radians().tan() / zoom).clamp(10.0, 256.0);

	Camera {
		view: Affine::translation([-center[0], -center[1], -distance]).mul(&world),
	}
}

/// skinview3d's zoom fitting `focus.fit`, and the point it centred.
fn frame(scene: &mut Scene, world: &Affine, focus: &Focus) -> Option<(f32, [f32; 2])> {
	let cosmetic = mesh_bounds(scene.build_mesh(world));
	let region = |regions: &[Region]| {
		regions
			.iter()
			.filter_map(|region| match region {
				Region::Head => boxes_bounds(world, &HEAD),
				Region::Body => boxes_bounds(world, &BODY),
				Region::Cosmetic => cosmetic,
			})
			.reduce(union)
	};

	let fit = region(focus.fit)?;
	let center = match focus.center {
		Center::Anchor(anchor) => {
			let anchor = region(anchor)?;
			[(anchor[0] + anchor[2]) / 2.0, (anchor[1] + anchor[3]) / 2.0]
		}
		Center::FitX(share) => [(fit[0] + fit[2]) / 2.0 * share, 0.0],
	};

	let aspect = WIDTH as f32 / HEIGHT as f32;
	let zoom_h = 33.0 / ((fit[3] - fit[1]) * focus.margin - 4.2).max(0.1);
	let zoom_w = 33.0 / ((fit[2] - fit[0]) * focus.margin / aspect - 4.2).max(0.1);
	Some((zoom_h.min(zoom_w).clamp(0.5, 2.6), center))
}

fn mesh_bounds(mesh: &Mesh) -> Option<Bounds> {
	mesh.runs
		.iter()
		.filter(|run| run.texture as usize == COSMETIC)
		// Four vertices of three floats per quad.
		.flat_map(|run| {
			mesh.positions[run.start as usize * 12..(run.start + run.count) as usize * 12]
				.as_chunks::<3>()
				.0
		})
		.map(|&point| point_bounds(point))
		.reduce(union)
}

/// Over each box's turned corners, as three.js bounds a mesh.
fn boxes_bounds(world: &Affine, boxes: &[[[f32; 3]; 2]]) -> Option<Bounds> {
	boxes
		.iter()
		.flat_map(|[min, max]| {
			(0..8).map(move |corner| {
				[0, 1, 2].map(|axis| {
					if corner >> axis & 1 == 0 {
						min[axis]
					} else {
						max[axis]
					}
				})
			})
		})
		.map(|corner| point_bounds(world.point(corner)))
		.reduce(union)
}

fn point_bounds([x, y, _]: [f32; 3]) -> Bounds {
	[x, y, x, y]
}

fn union(a: Bounds, b: Bounds) -> Bounds {
	[a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// One image per time.
fn frames(
	scene: &mut Scene,
	camera: &Camera,
	textures: &[Option<RgbaImage>],
	times: &[f32],
) -> Vec<RgbaImage> {
	let viewport = [WIDTH as f32, HEIGHT as f32];
	// plusrenderer's FOV is fixed, so widen it to skinview3d's about the centre.
	let scale = (RENDERER_FOV_Y / 2.0).to_radians().tan() / (FOV_Y / 2.0).to_radians().tan();
	let framing = (scale, viewport.map(|size| size / 2.0 * (1.0 - scale)));

	times
		.iter()
		.map(|&time| {
			scene.pose(time);
			rasterize(scene.build(camera, viewport), textures, framing)
		})
		.collect()
}

/// Draws the back-to-front runs with no depth test, as the core expects.
fn rasterize(
	buffers: &Buffers,
	textures: &[Option<RgbaImage>],
	framing: (f32, [f32; 2]),
) -> RgbaImage {
	let mut canvas = RgbaImage::new(WIDTH, HEIGHT);

	for run in &buffers.runs {
		let Some(Some(texture)) = textures.get(run.texture as usize) else {
			continue;
		};
		let indices = &buffers.indices[run.start as usize..(run.start + run.count) as usize];
		for triangle in indices.as_chunks::<3>().0 {
			fill(&mut canvas, buffers, triangle, texture, framing);
		}
	}

	canvas
}

fn fill(
	canvas: &mut RgbaImage,
	buffers: &Buffers,
	triangle: &[u16; 3],
	texture: &RgbaImage,
	(scale, offset): (f32, [f32; 2]),
) {
	let corner = |values: &[f32], index: u16| {
		let i = usize::from(index) * 2;
		[values[i], values[i + 1]]
	};
	let points = triangle.map(|i| {
		let [x, y] = corner(&buffers.positions, i);
		[x * scale + offset[0], y * scale + offset[1]]
	});
	let uvs = triangle.map(|i| corner(&buffers.uvs, i));
	let [a, b, c] = points;
	let area = edge(a, b, c);
	if area.abs() < f32::EPSILON {
		return;
	}

	// 0xAARRGGBB, flat per face.
	let [shade_a, shade_r, shade_g, shade_b] =
		buffers.colors[usize::from(triangle[0])].to_be_bytes();
	let (texture_width, texture_height) = texture.dimensions();
	let bound = |axis: usize, limit: u32| {
		let low = points.iter().map(|p| p[axis]).fold(f32::MAX, f32::min);
		let high = points.iter().map(|p| p[axis]).fold(f32::MIN, f32::max);
		(low.floor().max(0.0) as u32)..(high.ceil().min(limit as f32) as u32)
	};

	for y in bound(1, HEIGHT) {
		for x in bound(0, WIDTH) {
			let p = [x as f32 + 0.5, y as f32 + 0.5];
			let weights = [edge(b, c, p), edge(c, a, p), edge(a, b, p)].map(|w| w / area);
			if weights.iter().any(|&w| w < 0.0) {
				continue;
			}

			let [u, v] = [0, 1].map(|axis| (0..3).map(|k| weights[k] * uvs[k][axis]).sum::<f32>());
			let texel = texture.get_pixel(
				((u * texture_width as f32) as u32).min(texture_width - 1),
				((v * texture_height as f32) as u32).min(texture_height - 1),
			);
			let [tr, tg, tb, ta] = texel.0;
			let src_alpha = f32::from(ta) * f32::from(shade_a) / 65025.0;
			if src_alpha <= 0.0 {
				continue;
			}

			let src = [(tr, shade_r), (tg, shade_g), (tb, shade_b)]
				.map(|(t, s)| f32::from(t) * f32::from(s) / 255.0);
			let dst = canvas.get_pixel_mut(x, y);
			let dst_alpha = f32::from(dst.0[3]) / 255.0;
			let out_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);
			for channel in 0..3 {
				let blended = (src[channel] * src_alpha
					+ f32::from(dst.0[channel]) * dst_alpha * (1.0 - src_alpha))
					/ out_alpha;
				dst.0[channel] = blended.round() as u8;
			}
			dst.0[3] = (out_alpha * 255.0).round() as u8;
		}
	}
}

fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
	(b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

#[cfg(test)]
mod tests {
	use std::io::Write;

	use webp_animation::Decoder;
	use zip::{ZipWriter, write::SimpleFileOptions};

	use super::*;

	const WAVE: &str = r#"{"animations":{"wave":{"loop":true,"animation_length":1.0,
		"bones":{"rightArm":{"rotation":{"0.0":[0,0,0],"0.5":[0,0,-120],"1.0":[0,0,0]}}}}}}"#;
	const HAT: &str = r#"{"format_version":"1.12.0","minecraft:geometry":[{
		"description":{"identifier":"geometry.hat","texture_width":16,"texture_height":16},
		"bones":[{"name":"brim","pivot":[0,24,0],
			"cubes":[{"origin":[-5,32,-5],"size":[10,1,10],"uv":[0,0]}]}]}]}"#;

	fn skin() -> RgbaImage {
		let mut skin = RgbaImage::new(64, 64);
		skin.pixels_mut().for_each(|p| p.0 = [200, 100, 50, 255]);
		skin
	}

	#[test]
	fn draws_the_player_framed_in_the_canvas() {
		let mut scene = Scene::default();
		scene.load_player(PLAYER, Arms::Wide);
		scene.pose(0.0);
		let camera = camera(&mut scene, &CosmeticType::Emote);
		let canvas = frames(&mut scene, &camera, &[Some(skin())], &[0.0]).remove(0);

		let opaque = canvas.pixels().filter(|p| p.0[3] == 255).count();
		let total = (WIDTH * HEIGHT) as usize;
		assert!(opaque > total / 10, "the player fills the frame, got {opaque}");
		assert_eq!(canvas.get_pixel(0, 0).0[3], 0, "corners stay clear");
	}

	#[test]
	fn hats_are_framed_as_skinview3d_did() {
		let mut scene = Scene::default();
		scene.load_player(PLAYER, Arms::Wide);
		scene.load_cosmetic(
			COSMETIC,
			bedrock::parse_one(HAT).unwrap(),
			RenderSlot::Hat,
			None,
			None,
		);
		scene.pose(0.0);

		// skinview3d's default: 4.5 + 16.5 / tan(25°) / 0.9 away, on the player's middle.
		let [.., x, _, _, _, y, _, _, _, z] = camera(&mut scene, &CosmeticType::Aura).view.0;
		assert!(x.abs() < 1e-4 && (y + 16.0).abs() < 1e-4, "{x}, {y}");
		assert!((z + 43.816).abs() < 1e-2, "{z}");

		let [.., x, _, _, _, y, _, _, _, z] = camera(&mut scene, &CosmeticType::Hat).view.0;
		assert!(z > -43.0, "closer than the default, got {z}");
		assert!(y < -21.0, "centred above the middle, got {y}");
		assert!(x.abs() < 1.0, "the head stays central, got {x}");
	}

	#[test]
	fn only_moving_cosmetics_get_a_preview_of_one_loop() {
		let mut emote = Cursor::new(Vec::new());
		let mut writer = ZipWriter::new(&mut emote);
		writer
			.start_file("wave.json", SimpleFileOptions::default())
			.unwrap();
		writer.write_all(WAVE.as_bytes()).unwrap();
		writer.finish().unwrap();

		let renders = draw(
			skin(),
			Arms::Wide,
			CosmeticType::Emote,
			None,
			0.5,
			emote.get_ref(),
		)
		.unwrap();
		let webp = renders.preview.expect("an emote moves");
		let frames: Vec<Vec<u8>> = Decoder::new(&webp)
			.unwrap()
			.into_iter()
			.map(|frame| frame.data().to_vec())
			.collect();
		assert_eq!(frames.len(), 20, "one second at 20 fps");
		assert_ne!(frames[0], frames[10], "the arm is up halfway through");

		let mut cape = Cursor::new(Vec::new());
		RgbaImage::new(64, 32)
			.write_to(&mut cape, ImageFormat::Png)
			.unwrap();
		let renders = draw(
			skin(),
			Arms::Wide,
			CosmeticType::Cape,
			None,
			0.5,
			cape.get_ref(),
		)
		.unwrap();
		assert!(renders.preview.is_none(), "a plain cape holds still");
	}
}
