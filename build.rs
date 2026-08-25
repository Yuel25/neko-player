use std::env;
use std::path::{Path, PathBuf};

fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile Slint UI");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let mpv_dir = manifest_dir.join("third_party").join("mpv");

    if env::var("CARGO_CFG_WINDOWS").is_ok() {
        let icon_resource = manifest_dir.join("assets").join("neko-player.res");
        println!("cargo:rerun-if-changed=assets/neko-player-icon.png");
        println!("cargo:rerun-if-changed=assets/neko-player.ico");
        println!("cargo:rerun-if-changed=assets/neko-player.res");
        println!("cargo:rustc-link-arg={}", icon_resource.display());
    }

    if !mpv_dir.join("libmpv-2.lib").exists() {
        panic!(
            "third_party/mpv/libmpv-2.lib not found.\n\
             Run the dependency setup first: download the mpv-dev package from \
             https://github.com/zhongfly/mpv-winbuild/releases and generate the \
             MSVC import library (see README.md)."
        );
    }

    // Link against the pre-generated MSVC import library (forwards to libmpv-2.dll).
    println!("cargo:rustc-link-search=native={}", mpv_dir.display());
    println!("cargo:rustc-link-lib=dylib=libmpv-2");
    println!(
        "cargo:rerun-if-changed={}",
        mpv_dir.join("libmpv-2.lib").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        mpv_dir.join("libmpv-2.dll").display()
    );

    // Copy libmpv-2.dll next to the executable so the app runs without global PATH setup.
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out  →  exe dir is 3 levels up.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let exe_dir = out_dir.ancestors().nth(3).unwrap().to_path_buf();
    let src_dll = mpv_dir.join("libmpv-2.dll");
    let dst_dll = exe_dir.join("libmpv-2.dll");
    copy_if_changed(&src_dll, &dst_dll);
}

fn copy_if_changed(src: &Path, dst: &Path) {
    let needs_copy = match (std::fs::read(src), std::fs::read(dst)) {
        (Ok(source), Ok(destination)) => source != destination,
        _ => true,
    };
    if needs_copy {
        std::fs::copy(src, dst).unwrap_or_else(|e| panic!("failed to copy {}: {e}", src.display()));
    }
}
