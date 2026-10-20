//! Tauri 构建脚本。
//!
//! 额外职责：确保 `frontendDist` 目录存在。
//! `tauri::generate_context!()` 会在**编译期**检查这个目录，缺失就直接 panic。
//!
//! 分两种情况处理：
//! - **debug/test**：单元测试根本不需要前端，写一个占位页让 `cargo test` 能跑；
//! - **release**：正式包里绝不能嵌占位页（界面会是一片空白），直接报错要求先
//!   构建前端，把问题暴露在构建阶段而不是用户第一次打开时。

use std::path::Path;

fn main() {
    ensure_frontend_dist();
    tauri_build::build()
}

fn ensure_frontend_dist() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置");
    let dist = Path::new(&manifest).join("../dist");
    let index = dist.join("index.html");
    if index.exists() {
        return;
    }

    let release = std::env::var("PROFILE")
        .map(|p| p == "release")
        .unwrap_or(false);
    if release {
        panic!(
            "缺少前端产物 dist/index.html。release 构建不允许嵌入占位页。\n\
             请先运行：pnpm install && pnpm build\n\
             （或直接使用 pnpm tauri build，它会自动先构建前端）"
        );
    }

    if std::fs::create_dir_all(&dist).is_ok() {
        let _ = std::fs::write(
            index,
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>light-capture</title></head>\
             <body><p>前端资源尚未构建。请运行 <code>pnpm install &amp;&amp; pnpm build</code>。</p></body></html>",
        );
        println!(
            "cargo:warning=dist/index.html 不存在，已生成占位页（仅用于测试；完整界面请先运行 pnpm build）"
        );
    }
    println!("cargo:rerun-if-changed=../dist");
}
