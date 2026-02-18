use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    let bindings_path = manifest_dir.join("../vectorscan-rs-sys/src/bindings.rs");
    let bindings = fs::read_to_string(&bindings_path)
        .expect("Failed to read vectorscan-rs-sys bindings");

    let exports: Vec<&str> = bindings
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let name = trimmed.strip_prefix("pub fn ")?;
            let end = name.find('(')?;
            Some(&name[..end])
        })
        .filter(|name| name.starts_with("hs_"))
        .collect();

    assert!(
        !exports.is_empty(),
        "No hs_* exports found in bindings -- did the bindings format change?"
    );

    let def_content = format!(
        "LIBRARY vs\nEXPORTS\n{}\n",
        exports
            .iter()
            .map(|s| format!("    {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let def_path = out_dir.join("vs.def");
    fs::write(&def_path, &def_content).expect("Failed to write .def file");

    println!("cargo:rustc-cdylib-link-arg={}", def_path.display());
    println!("cargo:rerun-if-changed=build.rs");
}
