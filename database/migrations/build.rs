use std::{env, fs, path::PathBuf};

fn main() {
	let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
	println!("cargo:rerun-if-changed={}", src.display());

	let mut names: Vec<String> = fs::read_dir(&src)
		.unwrap()
		.filter_map(|entry| {
			let name = entry.unwrap().file_name().into_string().unwrap();
			let stem = name.strip_suffix(".rs")?;
			stem.starts_with("m2").then(|| stem.to_owned())
		})
		.collect();
	names.sort();

	let mut out = String::new();
	for name in &names {
		let path = src.join(format!("{name}.rs"));
		out += &format!("#[path = {:?}]\nmod {name};\n", path.display().to_string());
	}
	out += "fn discovered_migrations() -> Vec<Box<dyn MigrationTrait>> {\n\tvec![\n";
	for name in &names {
		out += &format!("\t\tBox::new({name}::Migration),\n");
	}
	out += "\t]\n}\n";

	let dest = PathBuf::from(env::var("OUT_DIR").unwrap()).join("migrations.rs");
	fs::write(dest, out).unwrap();
}
