// SPDX-License-Identifier: GPL-3.0-or-later

//! `winbridge-gfx-vkgen`: generates the Vulkan serializers, host dispatcher and ICD shims from
//! the Khronos registry for the command subset listed in `subset.rs`.
//!
//! Usage: `winbridge-gfx-vkgen [--check] [--registry PATH]`. Without `--check` the two
//! generated files are rewritten; with it the process fails if either differs from what the
//! registry would produce (drift detection).

mod emit;
mod registry;
mod subset;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_REGISTRY: &str = "/usr/share/vulkan/registry/vk.xml";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn rustfmt(src: &str) -> String {
    use std::io::Write;
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2021", "--emit", "stdout"])
        .env("RUSTFMT_CONFIG", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("rustfmt is required to format the generated code");
    let mut stdin = child.stdin.take().unwrap();
    let input = src.to_string();
    let t = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().unwrap();
    t.join().unwrap().unwrap();
    assert!(out.status.success(), "rustfmt rejected the generated code");
    String::from_utf8(out.stdout).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let reg_path = args
        .iter()
        .position(|a| a == "--registry")
        .map(|i| args[i + 1].clone())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
    let xml = std::fs::read_to_string(&reg_path).unwrap_or_else(|e| panic!("read {reg_path}: {e}"));
    let reg = registry::Registry::load(&xml);
    if args.iter().any(|a| a == "--survey") {
        emit::survey(&reg);
        return;
    }
    let out = emit::generate(&reg);

    let root = workspace_root();
    let files = [
        (root.join("crates/winbridge-gfx-vk/src/generated.rs"), rustfmt(&out.vk)),
        (root.join("crates/winbridge-gfx-icd/src/shims.rs"), rustfmt(&out.shims)),
    ];
    let mut drift = false;
    for (path, text) in &files {
        if check {
            if std::fs::read_to_string(path).ok().as_deref() != Some(text.as_str()) {
                eprintln!("drift: {} differs from the registry output", path.display());
                drift = true;
            }
        } else {
            std::fs::write(path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
            println!("wrote {} ({} bytes)", path.display(), text.len());
        }
    }
    if drift {
        std::process::exit(1);
    }
    if check {
        println!("generated files match vk.xml (VK_HEADER_VERSION {})", reg.header_version);
    }
}
