//! Embed the stdlib image layer directories (ADR 0062) as release data. Every
//! subdirectory of `stdlib/image-layers/` becomes one entry holding each of its
//! files by relative path, with the executable bit where the platform reports
//! one. Files directly under `stdlib/image-layers/` (such as README.md) are not
//! layers and are skipped.
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));
    let root = manifest_dir.join("../../stdlib/image-layers");
    println!("cargo::rerun-if-changed={}", root.display());
    let mut layers = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(&root).unwrap_or_else(|error| panic!("read {}: {error}", root.display())) {
            let path = entry.unwrap_or_else(|error| panic!("read {}: {error}", root.display())).path();
            if path.is_dir() {
                layers.push(path);
            }
        }
    }
    layers.sort();
    let mut generated = String::from("&[\n");
    for layer in layers {
        let name = layer.file_name().and_then(|name| name.to_str()).expect("layer directory names are UTF-8");
        let mut files = Vec::new();
        collect(&layer, &layer, &mut files);
        files.sort();
        writeln!(generated, "    EmbeddedLayerDirectory {{ directory: {name:?}, files: &[").expect("write to string");
        for (relative, path, executable) in files {
            let path = path.canonicalize().unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()));
            writeln!(generated, "        ({relative:?}, include_bytes!({:?}), {executable}),", path.display().to_string())
                .expect("write to string");
        }
        generated.push_str("    ] },\n");
    }
    generated.push(']');
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("stdlib_image_layers.rs");
    std::fs::write(&out, generated).unwrap_or_else(|error| panic!("write {}: {error}", out.display()));
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf, bool)>) {
    for entry in std::fs::read_dir(directory).unwrap_or_else(|error| panic!("read {}: {error}", directory.display())) {
        let path = entry.unwrap_or_else(|error| panic!("read {}: {error}", directory.display())).path();
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("collected paths are below the layer directory")
            .components()
            .map(|component| component.as_os_str().to_str().expect("layer file names are UTF-8"))
            .collect::<Vec<_>>()
            .join("/");
        files.push((relative, path.clone(), executable(&path)));
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> bool {
    false
}
