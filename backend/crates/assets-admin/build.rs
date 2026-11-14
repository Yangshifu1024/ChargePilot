//! 构建期产物准备（票 22）：前端产物目录存在则复制到本 crate 的 assets/
//! （rust-embed 嵌入 folder），镜像构建可直接预置 assets/；产物缺失时保留
//! 现有 assets（本地开发为空目录容忍，占位缺失容忍）。产物变更只触发本
//! crate 重编，业务代码不重编。
//!
//! 构建脚本豁免：`CARGO_MANIFEST_DIR` 是 cargo 注入的构建环境变量，不属于
//! 票 16 业务进程 env 禁令（disallowed-methods）管辖；`expect` 失败即构建
//! 环境损坏，中止构建是正确行为。
#![allow(clippy::disallowed_methods, clippy::expect_used)]

use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("../../frontend/admin-web/dist");
    let assets = Path::new(&manifest).join("assets");
    if dist.exists() {
        let _ = std::fs::remove_dir_all(&assets);
        let _ = std::fs::create_dir_all(&assets);
        copy_dir(&dist, &assets);
    } else {
        // 产物目录缺失：保留预置 assets/（镜像构建路径）或空目录（本地开发）。
        let _ = std::fs::create_dir_all(&assets);
    }
    println!("cargo:rerun-if-changed={}", dist.display());
}

fn copy_dir(src: &Path, dst: &Path) {
    for entry in std::fs::read_dir(src).expect("read dist") {
        let entry = entry.expect("entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            let _ = std::fs::create_dir_all(&target);
            copy_dir(&entry.path(), &target);
        } else if std::fs::copy(entry.path(), &target).is_err() {
            // 单文件复制失败容忍（损坏产物不入嵌入）。
        }
    }
}
