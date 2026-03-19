use std::{
    env,
    path::{Path, PathBuf},
};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ghostty-vt feature requires Linux or macOS");

fn main() {
    if env::var_os("CARGO_FEATURE_GHOSTTY_VT").is_none() {
        return;
    }

    println!("cargo:rerun-if-env-changed=CLEAT_GHOSTTY_PREFIX");

    let prefix = ghostty_prefix().unwrap_or_else(|| {
        panic!("ghostty-vt feature requires libghostty-vt. Set CLEAT_GHOSTTY_PREFIX or install it under ~/.local/opt/libghostty-vt")
    });
    let include_dir = prefix.join("include");
    let lib_dir = prefix.join("lib");
    let header = include_dir.join("ghostty").join("vt.h");
    let library = lib_dir.join(library_filename());

    assert!(header.exists(), "missing ghostty header at {}", header.display());
    assert!(library.exists(), "missing ghostty library at {}", library.display());

    println!("cargo:rustc-env=CLEAT_GHOSTTY_PREFIX={}", prefix.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}

fn ghostty_prefix() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("CLEAT_GHOSTTY_PREFIX").map(PathBuf::from) {
        return Some(explicit);
    }

    let home = env::var_os("HOME").map(PathBuf::from)?;
    [home.join(".local/opt/libghostty-vt")].into_iter().find(|path| is_valid_prefix(path))
}

fn is_valid_prefix(path: &Path) -> bool {
    path.join("include/ghostty/vt.h").exists() && path.join("lib").exists()
}

fn library_filename() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "libghostty-vt.so"
    }
    #[cfg(target_os = "macos")]
    {
        "libghostty-vt.dylib"
    }
}
