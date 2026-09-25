//! Z-Accounts Web 版入口：在 Termux / Linux 上以本地 HTTP 服务器形式
//! 复用上游 `src-tauri/src` 的纯逻辑模块（账号库 / 额度 / 领取 / OAuth / 加密备份）。
//!
//! 通过 `#[path]` 直接 include 上游模块文件，保持单一事实源：
//! 上游更新时无需同步拷贝（只需保持 oauth.rs / store.rs / Cargo.toml 的三个小补丁）。

mod api;
mod events;
mod httpsrv;

#[path = "../../src-tauri/src/cipher.rs"]
pub mod cipher;
#[path = "../../src-tauri/src/claim.rs"]
pub mod claim;
#[path = "../../src-tauri/src/flowlog.rs"]
pub mod flowlog;
#[path = "../../src-tauri/src/i18n.rs"]
pub mod i18n;
#[path = "../../src-tauri/src/model_status.rs"]
pub mod model_status;
#[path = "../../src-tauri/src/oauth.rs"]
pub mod oauth;
#[path = "../../src-tauri/src/quota.rs"]
pub mod quota;
#[path = "../../src-tauri/src/store.rs"]
pub mod store;
#[path = "../../src-tauri/src/zcrypto.rs"]
pub mod zcrypto;

use serde_json::json;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use events::Hub;

/// 全局账号库写锁（与上游 lib.rs 的 STORE_LOCK 同源；store.rs 内部经
/// `crate::store_guard()` 使用，故必须定义在 crate 根）。
static STORE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    match STORE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| -> Option<String> {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == name {
                return it.next().cloned();
            }
        }
        None
    };
    let port: u16 = flag("--port").and_then(|p| p.parse().ok()).unwrap_or(8790);
    let host = flag("--host").unwrap_or_else(|| "127.0.0.1".into());
    let root = frontend_root(&args);

    // 与桌面版一致的初始化：语言、流程日志、自动切换监控。
    let paths = store::Paths::detect();
    i18n::init_from_settings(&store::load_settings(&paths));
    flowlog::init(&paths.store_dir());

    let hub = Arc::new(Hub::new());
    api::start_auto_switch_monitor(hub.clone());

    let listener = match TcpListener::bind((host.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {host}:{port} 失败: {e}");
            std::process::exit(1);
        }
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| format!("{host}:{port}"));
    eprintln!("Z-Accounts Web 版已启动: http://{addr}/");
    eprintln!("前端目录: {}", root.display());
    eprintln!("账号库: {}", paths.store_dir().display());

    let router: httpsrv::Router = {
        let root = root.clone();
        let hub = hub.clone();
        Arc::new(move |req: &httpsrv::Request| route(req, &root, &hub))
    };
    httpsrv::serve(listener, router);
}

/// 解析前端静态根目录（含 index.html 的目录 = 仓库根）。
/// 优先级：--root 参数 > $ZACCOUNTS_WEB_ROOT > 当前目录 > 可执行文件 ../../（web/ 的上级）。
fn frontend_root(args: &[String]) -> PathBuf {
    if let Some(pos) = args.iter().position(|a| a == "--root") {
        if let Some(v) = args.get(pos + 1) {
            let p = PathBuf::from(v);
            if p.join("index.html").is_file() {
                return p;
            }
        }
    }
    let mut candidates: Vec<PathBuf> = vec![];
    if let Ok(v) = std::env::var("ZACCOUNTS_WEB_ROOT") {
        candidates.push(PathBuf::from(v));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        // web/target/{debug,release}/zaccounts-web → 仓库根
        if let Some(parent) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            candidates.push(parent.to_path_buf());
        }
    }
    for c in candidates {
        if c.join("index.html").is_file() {
            return c;
        }
    }
    PathBuf::from(".")
}

fn route(req: &httpsrv::Request, root: &PathBuf, hub: &Arc<Hub>) -> httpsrv::Reply {
    let path = req.path.as_str();

    // 前端 emit（captcha://interactive 等）：转发给事件总线，广播到所有 SSE 客户端。
    if req.method == "POST" && path == "/api/emit" {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&req.body) {
            let event = v
                .get("event")
                .and_then(|e| e.as_str())
                .unwrap_or_default()
                .to_string();
            let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
            hub.emit(&event, &payload);
        }
        return httpsrv::Reply::respond(httpsrv::Response::json_ok(json!({})));
    }

    // SSE 事件流（对应桌面版的 app.emit → 前端 listen）。
    if req.method == "GET" && (path == "/api/events" || path == "/api/events/") {
        let hub = hub.clone();
        return httpsrv::Reply::take_over(Box::new(move |stream| {
            events::run_sse(stream, &hub);
        }));
    }

    // 后端命令 API（对应桌面版的 invoke 命令，参数命名同为 camelCase）。
    if req.method == "POST" {
        if let Some(cmd) = path.strip_prefix("/api/") {
            let body = serde_json::from_slice::<serde_json::Value>(&req.body)
                .unwrap_or_else(|_| json!({}));
            let cmd = cmd.trim_end_matches('/').to_string();
            return match api::dispatch(&cmd, &body, hub) {
                Ok(v) => httpsrv::Reply::respond(httpsrv::Response::json_ok(v)),
                Err(e) => httpsrv::Reply::respond(httpsrv::Response::json_err(&e)),
            };
        }
    }

    // 静态前端文件。
    if req.method == "GET" || req.method == "HEAD" {
        if let Some(resp) = serve_static(path, root) {
            return httpsrv::Reply::respond(resp);
        }
    }

    httpsrv::Reply::respond(httpsrv::Response::text(404, "not found"))
}

/// 静态文件白名单映射（vite dev 语义）：
///   /                → index.html
///   /index.html /settings.html /captcha.html → 仓库根
///   /brand-icon.png  → public/brand-icon.png
///   /src/**          → 仓库根 src/**（原生 ESM，无需构建）
fn serve_static(path: &str, root: &PathBuf) -> Option<httpsrv::Response> {
    if path.contains("..") || path.contains('\\') {
        return None;
    }
    let rel = match path {
        "/" | "/index.html" => "index.html".to_string(),
        "/settings.html" | "/captcha.html" => path.trim_start_matches('/').to_string(),
        "/brand-icon.png" => "public/brand-icon.png".to_string(),
        _ => {
            let p = path.trim_start_matches('/');
            if p.starts_with("src/") || p.starts_with("public/") {
                p.to_string()
            } else {
                return None;
            }
        }
    };
    let full = root.join(&rel);
    let full = std::fs::canonicalize(&full).ok()?;
    let root_canon = std::fs::canonicalize(root).ok()?;
    if !full.starts_with(&root_canon) || !full.is_file() {
        return None;
    }
    let data = std::fs::read(&full).ok()?;
    let ct = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    let mut resp = httpsrv::Response::bytes(200, ct, data);
    resp.set_header("Cache-Control", "no-store");
    Some(resp)
}
