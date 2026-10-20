//! Tauri 构建脚本。
//!
//! 额外职责：确保 `frontendDist` 目录存在。
//! `tauri::generate_context!()` 会在**编译期**检查这个目录，缺失就直接 panic。
//! 这导致 `cargo test` / `cargo build`（不经过 tauri CLI）在全新 clone 上必然失败——
//! 而单元测试根本不需要前端产物。这里放一个占位页兜底：真正的界面由
//! `pnpm tauri build`（会先跑 `pnpm build`）覆盖，占位文件只在跳过前端构建时可见。

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
    if std::fs::create_dir_all(&dist).is_ok() {
        let _ = std::fs::write(
            index,
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>light-capture</title></head>\
             <body><p>前端资源尚未构建。请运行 <code>pnpm install &amp;&amp; pnpm build</code>。</p></body></html>",
        );
        println!(
            "cargo:warning=dist/index.html 不存在，已生成占位页（完整界面请先运行 pnpm build）"
        );
    }
    println!("cargo:rerun-if-changed=../dist");
}
