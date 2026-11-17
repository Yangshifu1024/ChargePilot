//! 用户端 H5 产物（编译期嵌入；缺失容忍=空集合）。
//!
//! 独立叶子 crate：嵌入宏展开昂贵，隔离后产物变更只重编本 crate 与最终
//! 二进制链接，业务代码不重编；不依赖 platform（SPA 回退所需常量由装配根
//! 传参）。本地无前端产物时 build.rs 建空 assets/ 容忍缺失（嵌入空集合），
//! 增量构建与 CI 冒烟不受影响。

use rust_embed::RustEmbed;

/// 嵌入根：/index.html 等以完整相对路径检索。
#[derive(RustEmbed)]
#[folder = "assets"]
pub struct Assets;

/// 按路径取嵌入文件字节（None = 未命中，交给 SPA 回退或 404）。
#[must_use]
pub fn get(path: &str) -> Option<Vec<u8>> {
    Assets::get(path).map(|f| f.data.into_owned())
}

/// 产物是否真实嵌入（本地开发无产物时为 false，静态路由不挂载）。
#[must_use]
pub fn is_empty() -> bool {
    Assets::iter().next().is_none()
}

/// index.html 字节（SPA 回退正文）。
#[must_use]
pub fn index() -> Option<Vec<u8>> {
    get("index.html")
}
