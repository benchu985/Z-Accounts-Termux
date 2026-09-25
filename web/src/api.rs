//! 命令分发层：37 个 Tauri invoke 命令 → HTTP POST /api/<cmd>。
//!
//! 命令体逐一移植自上游 `src-tauri/src/lib.rs` 的 `#[tauri::command]` 函数，
//! 差异只有三处（均与窗口/托盘相关）：
//!  - emit(app, ...) → hub.emit(...)（SSE 广播）
//!  - rebuild_tray → 删除
//!  - 文件对话框命令 → Web 等价物（导出写入手机下载目录 / 导入接收上传内容）

use crate::store::*;
use crate::{cipher, claim, events::Hub, i18n, model_status, oauth, quota, store, store_guard};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct PendingClaim {
    account_id: String,
    account_name: String,
    plan_id: String,
    plan_name: String,
    credentials: Value,
    config: Option<Value>,
    device_mid: String,
}

static PENDING_CLAIM: Mutex<Option<PendingClaim>> = Mutex::new(None);

fn pending_guard() -> std::sync::MutexGuard<'static, Option<PendingClaim>> {
    match PENDING_CLAIM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct PendingOAuth {
    provider: String,
    state: String,
    flow: String,
}

#[derive(Clone)]
struct PollCfg {
    url: String,
    token: String,
    expires_at_ms: u128,
    interval_ms: u64,
}

static PENDING_OAUTH: Mutex<Option<PendingOAuth>> = Mutex::new(None);

fn pending_oauth_guard() -> std::sync::MutexGuard<'static, Option<PendingOAuth>> {
    match PENDING_OAUTH.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

/// Web 版导出目标目录：优先手机共享存储下载目录，最后退回家目录下 downloads/。
fn download_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let home_pb = PathBuf::from(&home);
    let candidates = [
        home_pb.join("storage/downloads"), // termux-setup-storage 后的快捷方式
        PathBuf::from("/storage/emulated/0/Download"),
        home_pb.join("downloads"),
    ];
    for c in candidates {
        if c.is_dir() {
            return c;
        }
    }
    let c = home_pb.join("downloads");
    let _ = std::fs::create_dir_all(&c);
    c
}

// ---------------- 参数提取（前端 invoke 传 camelCase 键） ----------------

fn get_str(b: &Value, key: &str) -> Result<String, String> {
    b.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("missing param: {key}"))
}

fn get_bool(b: &Value, key: &str) -> Result<bool, String> {
    b.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| format!("missing param: {key}"))
}

fn opt_str(b: &Value, key: &str) -> Option<String> {
    b.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn opt_bool(b: &Value, key: &str) -> Option<bool> {
    b.get(key).and_then(|v| v.as_bool())
}

// ---------------- 分发入口 ----------------

pub fn dispatch(cmd: &str, b: &Value, hub: &Arc<Hub>) -> Result<Value, String> {
    match cmd {
        // ===== 账号库 =====
        "get_state" => to_value(store::get_state(&Paths::detect())?),
        "app_version" => Ok(json!(env!("CARGO_PKG_VERSION"))),

        "capture_current" => {
            let name = opt_str(b, "name");
            let _guard = store_guard();
            to_value(store::capture_current(&Paths::detect(), name)?)
        }
        "rename_account" => {
            let id = get_str(b, "id")?;
            let name = get_str(b, "name")?;
            let _guard = store_guard();
            to_value(store::rename_account(&Paths::detect(), &id, &name)?)
        }
        "delete_account" => {
            let id = get_str(b, "id")?;
            let _guard = store_guard();
            store::delete_account(&Paths::detect(), &id)?;
            Ok(Value::Null)
        }
        "update_account_from_live" => {
            let id = get_str(b, "id")?;
            let _guard = store_guard();
            to_value(store::update_account_from_live(&Paths::detect(), &id)?)
        }
        "switch_to" => {
            let id = get_str(b, "id")?;
            let force = get_bool(b, "force")?;
            let restart = get_bool(b, "restart")?;
            let _guard = store_guard();
            let paths = Paths::detect();
            let hot = load_settings(&paths).hot_switch();
            to_value(store::switch_to(&paths, &id, force, restart, hot)?)
        }

        // ===== 额度 =====
        "get_live_quota" => to_value(store::live_quota(&Paths::detect())?),
        "get_account_quota" => {
            let id = get_str(b, "id")?;
            to_value(store::account_quota(&Paths::detect(), &id)?)
        }
        "get_recent_model_status" => Ok(match model_status::recent(&Paths::detect()) {
            Some(m) => serde_json::to_value(m).unwrap_or(Value::Null),
            None => Value::Null,
        }),

        // ===== 领取额度 =====
        "claim_preview" => {
            let id = get_str(b, "id")?;
            let paths = Paths::detect();
            let mid = store::ensure_virtual_device_mid(&paths, &id)?;
            let acc = load_account(&paths, &id)?;
            to_value(claim::preview_plans(
                &paths.home,
                &acc.credentials,
                acc.config.as_ref(),
                Some(mid),
            )?)
        }
        "claim_refresh" => {
            let id = get_str(b, "id")?;
            let paths = Paths::detect();
            let mid = store::ensure_virtual_device_mid(&paths, &id)?;
            let acc = load_account(&paths, &id)?;
            // 遥测激活已移除：实测对 Start Plan 授予无效（服务端条件授予，不存在
            // claim/activate 端点，见逆向分析 docs/auth/activation-protocol.md）；
            // 伪造 app_launch/app_daily_active 事件只带来隐私暴露与请求开销。
            let plans = claim::preview_plans(
                &paths.home,
                &acc.credentials,
                acc.config.as_ref(),
                Some(mid),
            )?;
            #[derive(serde::Serialize)]
            #[serde(rename_all = "camelCase")]
            struct ClaimRefreshResult {
                plans: Vec<claim::ClaimPlan>,
                activated: bool,
                activation_error: Option<String>,
            }
            to_value(ClaimRefreshResult {
                plans,
                activated: false,
                activation_error: None,
            })
        }
        "claim_start" => {
            let id = get_str(b, "id")?;
            let plan_id = get_str(b, "planId")?;
            let paths = Paths::detect();
            let mid = store::ensure_virtual_device_mid(&paths, &id)?;
            let acc = load_account(&paths, &id)?;
            let plans = claim::preview_plans(
                &paths.home,
                &acc.credentials,
                acc.config.as_ref(),
                Some(mid.clone()),
            )?;
            let plan = plans
                .iter()
                .find(|p| p.plan_id == plan_id)
                .ok_or_else(|| i18n::tr("err.claim.gone"))?;
            let display = if plan.name.is_empty() {
                plan.plan_id.clone()
            } else {
                plan.name.clone()
            };
            *pending_guard() = Some(PendingClaim {
                account_id: acc.id.clone(),
                account_name: acc.name.clone(),
                plan_id: plan.plan_id.clone(),
                plan_name: display.clone(),
                credentials: acc.credentials,
                config: acc.config,
                device_mid: mid,
            });
            // 桌面版在此打开 captcha 子窗口；Web 版由前端 invoke 成功后自开 captcha iframe。
            Ok(json!({ "account": acc.name, "plan": display }))
        }
        "claim_captcha_config" => to_value(claim::fetch_captcha_config()?),
        "claim_captcha_submit" => {
            let param = get_str(b, "param")?;
            let region = opt_str(b, "region");
            let pending = pending_guard()
                .take()
                .ok_or_else(|| i18n::tr("err.claim.none_pending"))?;
            let paths = Paths::detect();
            let res = claim::submit_claim(
                &paths.home,
                &pending.credentials,
                pending.config.as_ref(),
                &pending.plan_id,
                &param,
                region.as_deref(),
                Some(pending.device_mid),
            );
            match res {
                Ok(v) => {
                    let ms = |k: &str| -> Option<i64> {
                        v.pointer(&format!("/data/plan/{k}"))
                            .and_then(|x| x.as_i64())
                            .map(|s| s * 1000)
                    };
                    let server_time = v
                        .pointer("/data/server_time")
                        .and_then(|x| x.as_i64())
                        .map(|s| s * 1000);
                    let outcome = claim::ClaimOutcome {
                        account_id: pending.account_id.clone(),
                        account_name: pending.account_name.clone(),
                        plan_name: pending.plan_name.clone(),
                        starts_at: ms("starts_at"),
                        ends_at: ms("ends_at"),
                        server_time,
                    };
                    let p = serde_json::to_value(&outcome).unwrap_or(Value::Null);
                    hub.emit("claim://result", &p);
                    Ok(p)
                }
                Err(e) => {
                    let p = claim::failure_payload(
                        &pending.account_id,
                        &pending.account_name,
                        &pending.plan_name,
                        &e,
                    );
                    hub.emit("claim://result", &p);
                    Ok(p)
                }
            }
        }
        "claim_cancel" => {
            *pending_guard() = None;
            Ok(Value::Null)
        }

        // ===== OAuth（Web 版走纯 poll 通道，无需深链窗口）=====
        "oauth_providers" => to_value(oauth::OAUTH_PROVIDERS.to_vec()),
        "oauth_begin" => oauth_begin(b),
        "set_auth_proxy" => {
            let on = get_bool(b, "on")?;
            let url = opt_str(b, "url");
            {
                let _guard = store_guard();
                let paths = Paths::detect();
                let trimmed = url.as_deref().map(str::trim).filter(|s| !s.is_empty());
                let normalized = match trimmed {
                    Some(s) => Some(oauth::parse_proxy_url(s)?),
                    None => None,
                };
                if on && normalized.is_none() {
                    return Err(i18n::tr("err.proxy.need_url"));
                }
                let mut s = load_settings(&paths);
                s.auth_proxy_on = Some(on);
                s.auth_proxy_url = normalized;
                save_settings(&paths, &s)?;
            }
            hub.emit("state-changed", &Value::Null);
            Ok(Value::Null)
        }

        // ===== 进程 / 行为 / 设置 =====
        "kill_zcode" => {
            let _guard = store_guard();
            if store::kill_zcode()? {
                Ok(Value::Null)
            } else {
                Err(i18n::tr("err.zcode.kill_timeout"))
            }
        }
        "set_behavior" => {
            let _guard = store_guard();
            let paths = Paths::detect();
            let mut s = load_settings(&paths);
            if let Some(v) = opt_bool(b, "launchAfterSwitch") {
                s.launch_after_switch = Some(v);
            }
            if let Some(v) = opt_bool(b, "closeToTray") {
                s.close_to_tray = Some(v);
            }
            if let Some(v) = opt_bool(b, "hotSwitch") {
                s.hot_switch = Some(v);
            }
            if let Some(v) = opt_bool(b, "autoClaim") {
                s.auto_claim = Some(v);
            }
            if let Some(v) = opt_bool(b, "autoSwitch") {
                s.auto_switch = Some(v);
            }
            let r = save_settings(&paths, &s);
            hub.emit("state-changed", &Value::Null);
            r?;
            Ok(Value::Null)
        }
        "set_language" => {
            let lang = get_str(b, "lang")?;
            let l = i18n::Lang::parse(&lang)
                .ok_or_else(|| i18n::trf("err.lang.unknown", &[("lang", &lang)]))?;
            {
                let _guard = store_guard();
                let paths = Paths::detect();
                let mut s = load_settings(&paths);
                s.language = Some(l.as_str().to_string());
                save_settings(&paths, &s)?;
            }
            i18n::set(l);
            hub.emit("state-changed", &Value::Null);
            Ok(Value::Null)
        }
        "set_theme" => {
            let theme = get_str(b, "theme")?;
            if theme != "light" && theme != "dark" {
                return Err("Unsupported theme".into());
            }
            {
                let _guard = store_guard();
                let paths = Paths::detect();
                let mut settings = load_settings(&paths);
                settings.theme = Some(theme);
                save_settings(&paths, &settings)?;
            }
            hub.emit("state-changed", &Value::Null);
            Ok(Value::Null)
        }

        // ===== 桌面专属命令的 Web 等价物 =====
        "autostart_status" => Ok(json!(false)), // Web 版不支持开机自启（用 termux-services 自行配置）
        "autostart_set" => Ok(json!(false)),
        "reveal_main" => Ok(Value::Null), // 桌面版用于取消主窗口隐藏；Web 无窗口
        "open_settings" => Ok(Value::Null), // Web 版由前端直接打开 /settings.html
        "open_external" => {
            let url = get_str(b, "url")?;
            store::open_url(&url)?;
            Ok(Value::Null)
        }

        // ===== 导出 / 导入（Web 等价物）=====
        "export_pick_path" => {
            let id = get_str(b, "id")?;
            let acc = load_account(&Paths::detect(), &id)?;
            let path = download_dir().join(format!("{}.zsb", sanitize_filename(&acc.name)));
            Ok(json!({ "picked": true, "path": path.to_string_lossy(), "name": acc.name }))
        }
        "export_finalize" => {
            let path = get_str(b, "path")?;
            let id = get_str(b, "id")?;
            let password = get_str(b, "password")?;
            export_write(&path, &[id], &password, false)
        }
        "export_all_pick_path" => {
            let accounts = list_accounts(&Paths::detect())?;
            if accounts.is_empty() {
                return Err(i18n::tr("err.export.empty"));
            }
            let path = download_dir().join("zcode-accounts.zsb");
            Ok(json!({ "picked": true, "path": path.to_string_lossy(), "count": accounts.len() }))
        }
        "export_all_finalize" => {
            let path = get_str(b, "path")?;
            let password = get_str(b, "password")?;
            export_write(&path, &[], &password, true)
        }
        // Web 版：前端用 <input type=file> 读取 .zsb 内容后上传，服务端做桌面版
        // import_pick_files 相同的 sealed 校验，返回相同形状，后续流程不变。
        "import_pick_files" => {
            let files = b
                .get("files")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "missing param: files".to_string())?;
            let mut sealed = vec![];
            let mut errors = vec![];
            for f in files {
                let fname = f
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let content = match f.get("content").and_then(|v| v.as_str()) {
                    Some(c) => c,
                    None => {
                        errors.push(i18n::trf(
                            "err.import.read",
                            &[("fname", fname.as_str()), ("e", "no content")],
                        ));
                        continue;
                    }
                };
                match serde_json::from_str::<Value>(content) {
                    Ok(v) => {
                        if !cipher::is_sealed(&v) {
                            errors.push(i18n::trf(
                                "err.import.not_sealed",
                                &[("fname", fname.as_str())],
                            ));
                        } else if v.get("format").and_then(|f| f.as_str())
                            != Some(cipher::FORMAT_BUNDLE)
                        {
                            errors.push(i18n::trf(
                                "err.import.not_bundle",
                                &[("fname", fname.as_str())],
                            ));
                        } else {
                            sealed.push((fname, v));
                        }
                    }
                    Err(e) => errors.push(i18n::trf(
                        "err.import.json",
                        &[("fname", fname.as_str()), ("e", &e.to_string())],
                    )),
                }
            }
            Ok(json!({ "picked": true, "sealed": sealed, "errors": errors }))
        }
        "import_sealed" => {
            let files: Vec<(String, Value)> =
                serde_json::from_value(b.get("files").cloned().unwrap_or(Value::Null))
                    .map_err(|_| "missing param: files".to_string())?;
            let password = get_str(b, "password")?;
            let _guard = store_guard();
            let mut decrypted = vec![];
            let mut errors = vec![];
            for (fname, v) in files {
                match cipher::open(&v, &password) {
                    Ok(payload) => decrypted.push((fname, payload)),
                    Err(e) => errors.push(i18n::trf(
                        "err.import.wrap",
                        &[("fname", fname.as_str()), ("e", &e)],
                    )),
                }
            }
            let mut report = if decrypted.is_empty() {
                ImportReport::default()
            } else {
                import_values(&Paths::detect(), &decrypted)?
            };
            report.picked = true;
            report.errors.extend(errors);
            to_value(report)
        }
        "pick_zcode_path" => Ok(json!({ "picked": false })), // Web 版设置页隐藏浏览按钮
        "set_zcode_path" => {
            let path = get_str(b, "path")?;
            let _guard = store_guard();
            let paths = Paths::detect();
            let mut s = load_settings(&paths);
            s.zcode_path = store::normalize_zcode_path(&path);
            let r = save_settings(&paths, &s);
            hub.emit("state-changed", &Value::Null);
            r?;
            Ok(Value::Null)
        }
        "launch_zcode" => {
            let paths = Paths::detect();
            let (p, ok) = store::effective_zcode_path(&paths);
            if !ok {
                return Err(i18n::trf("err.zcode.path_invalid_hint", &[("p", &p)]));
            }
            store::launch_zcode(&p)?;
            Ok(Value::Null)
        }

        other => Err(format!("unknown command: {other}")),
    }
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// 导出 .zsb：按桌面版 export_finalize / export_all_finalize 的逻辑写入指定路径。
fn export_write(path: &str, ids: &[String], password: &str, all: bool) -> Result<Value, String> {
    let accounts = if all {
        list_accounts(&Paths::detect())?
    } else {
        let id = ids.first().ok_or("missing id")?;
        vec![load_account(&Paths::detect(), id)?]
    };
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty_short"));
    }
    let payload = store::export_bundle_value(&accounts);
    let sealed = cipher::seal(&payload, password, cipher::FORMAT_BUNDLE)?;
    let body = serde_json::to_string_pretty(&sealed).unwrap_or_default() + "\n";
    store::atomic_write(std::path::Path::new(path), &body)
        .map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    if all {
        Ok(json!({ "saved": true, "path": path, "count": accounts.len() }))
    } else {
        Ok(json!({ "saved": true, "path": path }))
    }
}

// ---------------- OAuth poll 流（Web 版主通道） ----------------

fn oauth_begin(b: &Value) -> Result<Value, String> {
    let provider = get_str(b, "provider")?;
    if !oauth::OAUTH_PROVIDERS.iter().any(|p| p.id == provider) {
        return Err(i18n::trf(
            "err.oauth.unknown_provider",
            &[("provider", &provider)],
        ));
    }
    // 仅校验代理设置（poll 通道的 HTTP 请求由 ureq 直连，桌面版的代理只作用于登录窗口）
    if let Some(p) = load_settings(&Paths::detect()).auth_proxy() {
        oauth::parse_proxy_url(p)?;
    }
    *pending_oauth_guard() = None;
    let flow = uuid::Uuid::new_v4().to_string();
    crate::flowlog::log(&flow, "begin", &format!("provider={provider} channel=poll"));
    let mid = uuid::Uuid::new_v4().to_string();

    let init = oauth::init_flow(&provider, &mid).map_err(|e| {
        crate::flowlog::log(&flow, "init-fail", &e);
        e
    })?;
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let srv_flow = init.poll_url.rsplit('/').next().unwrap_or("");
        let expires_in = init.expires_at_ms.saturating_sub(now) / 1000;
        crate::flowlog::log(
            &flow,
            "init-ok",
            &format!(
                "server_flow={srv_flow} expires_in={expires_in}s interval={}ms",
                init.poll_interval_ms
            ),
        );
    }
    let poll_cfg = PollCfg {
        url: init.poll_url.clone(),
        token: init.poll_token.clone(),
        expires_at_ms: init.expires_at_ms,
        interval_ms: init.poll_interval_ms,
    };
    *pending_oauth_guard() = Some(PendingOAuth {
        provider: provider.clone(),
        state: init.state.clone(),
        flow: flow.clone(),
    });
    spawn_poll_loop(provider.clone(), flow.clone(), mid, poll_cfg);
    // Web 版不弹窗口：把登录链接交给前端（新标签页打开），结果经 SSE oauth://done 推送。
    Ok(json!({ "authorizeUrl": init.authorize_url, "provider": provider }))
}

/// 移植自 lib.rs::spawn_poll_loop，AppHandle → Arc<Hub>。
fn spawn_poll_loop(
    provider: String,
    flow: String,
    mid: String,
    cfg: PollCfg,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let ours = || {
            pending_oauth_guard()
                .as_ref()
                .map(|p| p.flow == flow)
                .unwrap_or(false)
        };
        let deadline = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            std::cmp::min(cfg.expires_at_ms, now + u128::from(oauth::FLOW_TIMEOUT_MS))
        };
        loop {
            if !ours() {
                crate::flowlog::log(&flow, "poll-exit", "flow-done-or-replaced");
                return;
            }
            match oauth::poll_flow_once(&cfg.url, &cfg.token, &mid) {
                Ok(oauth::PollOutcome::Pending) => {}
                Ok(oauth::PollOutcome::Ready(data)) => {
                    crate::flowlog::log(&flow, "poll-ready", "");
                    let raw = json!({ "code": 0, "data": data });
                    let result =
                        persist_oauth_account(&Paths::detect(), &provider, &raw, &flow, &mid, true);
                    match &result {
                        Ok(v) => crate::flowlog::log(
                            &flow,
                            "persist-ok",
                            &format!("channel=poll duplicate={}", v.get("duplicate").is_some()),
                        ),
                        Err(e) if e != "__superseded__" => {
                            crate::flowlog::log(
                                &flow,
                                "persist-fail",
                                &format!("channel=poll {e}"),
                            );
                        }
                        Err(_) => {}
                    }
                    finalize_oauth_result(&result);
                    return;
                }
                Err(e) => {
                    if !ours() {
                        crate::flowlog::log(&flow, "poll-exit", "superseded");
                        return;
                    }
                    crate::flowlog::log(&flow, "poll-fail", &e);
                    *pending_oauth_guard() = None;
                    finalize_oauth_result(&Err(e));
                    return;
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if now >= deadline {
                if !ours() {
                    return;
                }
                crate::flowlog::log(&flow, "poll-timeout", "");
                *pending_oauth_guard() = None;
                finalize_oauth_result(&Err(i18n::tr("err.oauth.expired")));
                return;
            }
            let sleep = (deadline - now).min(cfg.interval_ms as u128) as u64;
            std::thread::sleep(std::time::Duration::from_millis(sleep));
        }
    })
}

/// 移植自 lib.rs::finalize_oauth_result，emit 经 SSE 广播。
fn finalize_oauth_result(result: &Result<Value, String>) {
    if let Err(e) = result {
        if e == "__superseded__" || e == "__attribution__" {
            return;
        }
    }
    // finalize 不持有 Hub（静态上下文）——由调用方通过全局 hub 广播
    if let Some(h) = global_hub() {
        match result {
            Ok(v) => h.emit("oauth://done", v),
            Err(e) => h.emit("oauth://done", &json!({ "ok": false, "error": e })),
        }
    }
}

// 全局 Hub（供 poll 线程 / 自动切换线程广播使用，无需层层传参）
use std::sync::OnceLock;
static GLOBAL_HUB: OnceLock<Arc<Hub>> = OnceLock::new();
pub fn set_hub(hub: Arc<Hub>) {
    let _ = GLOBAL_HUB.set(hub);
}
fn global_hub() -> Option<Arc<Hub>> {
    GLOBAL_HUB.get().cloned()
}

/// 移植自 lib.rs::persist_oauth_account（业务体不变，仅移除 AppHandle）。
fn persist_oauth_account(
    paths: &Paths,
    provider: &str,
    raw: &Value,
    flow: &str,
    mid: &str,
    poll_ready: bool,
) -> Result<Value, String> {
    let flow_still_ours = || {
        pending_oauth_guard()
            .as_ref()
            .map(|p| p.flow == flow)
            .unwrap_or(false)
    };
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    let jwt = raw
        .pointer("/data/token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| i18n::tr("err.oauth.no_token"))?
        .to_string();
    let raw_access = oauth::extract_access_token(provider, raw).unwrap_or_default();
    let access_token = if provider == "zai" && !raw_access.is_empty() {
        oauth::resolve_zai_business_token(&raw_access)
            .ok_or_else(|| i18n::tr("err.oauth.zai_business"))?
    } else {
        raw_access
    };
    let userinfo = if poll_ready {
        oauth::extract_poll_user_profile(raw)
    } else {
        oauth::extract_user_profile(provider, raw)
    }
    .or_else(|| {
        (!access_token.is_empty())
            .then(|| oauth::fetch_userinfo(provider, &access_token))
            .flatten()
    });
    let refresh_token = oauth::extract_refresh_token(provider, raw);
    let credentials = oauth::assemble_credentials_with_token(
        provider,
        &jwt,
        userinfo.as_ref(),
        (!access_token.is_empty()).then_some(access_token.as_str()),
        refresh_token.as_deref(),
    );
    let config = oauth::assemble_config(provider, &jwt, &access_token);

    let _lock = store_guard();
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    let accounts = list_accounts(paths)?;
    let hash = canonical_hash(&credentials);
    if let Some(i) = store::find_same_login(&credentials, &hash, &accounts, &paths.home) {
        let mut dup = accounts[i].clone();
        dup.hash = hash.clone();
        dup.credentials = credentials;
        dup.config = Some(config);
        dup.updated_at = now_ts();
        if dup
            .virtual_device_mid
            .as_deref()
            .map_or(true, |m| m.trim().is_empty())
        {
            dup.virtual_device_mid = Some(mid.to_string());
        }
        if !flow_still_ours() {
            return Err("__superseded__".into());
        }
        save_account(paths, &dup)?;
        *pending_oauth_guard() = None;
        return Ok(
            json!({ "id": dup.id, "name": dup.name, "provider": provider, "duplicate": true }),
        );
    }
    let base = credentials
        .get(format!("oauth:{provider}:user_info"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|u| u.get("username").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_else(|| match provider {
            "zai" => "z.ai".to_string(),
            _ => "BigModel".to_string(),
        });
    let name = unique_name(&accounts, &base);
    let ts = now_ts();
    let acc = Account {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        created_at: ts.clone(),
        updated_at: ts,
        hash,
        credentials,
        config: Some(config),
        virtual_device_mid: Some(mid.to_string()),
        virtual_arms_uid: Some(store::new_arms_uid()),
    };
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    save_account(paths, &acc)?;
    *pending_oauth_guard() = None;
    Ok(json!({ "id": acc.id, "name": acc.name, "provider": provider }))
}

/// 移植自 lib.rs::start_auto_switch_monitor，AppHandle emit → SSE 广播。
pub fn start_auto_switch_monitor(hub: Arc<Hub>) {
    set_hub(hub);
    std::thread::spawn(move || {
        use std::time::{Duration, Instant};
        const CHECK_INTERVAL: Duration = Duration::from_secs(90);
        let mut last_check: Option<Instant> = None;
        let mut exhausted_once: Option<String> = None;
        let mut notified: Option<String> = None;

        loop {
            std::thread::sleep(Duration::from_secs(10));
            let paths = Paths::detect();
            if !load_settings(&paths).auto_switch() {
                last_check = None;
                exhausted_once = None;
                notified = None;
                continue;
            }
            if pending_guard().is_some()
                || last_check.is_some_and(|at| at.elapsed() < CHECK_INTERVAL)
            {
                continue;
            }
            last_check = Some(Instant::now());

            let Ok(snapshot) = store::get_state(&paths) else {
                exhausted_once = None;
                continue;
            };
            let Some(active_id) = snapshot.active_account_id.clone() else {
                exhausted_once = None;
                continue;
            };
            if !snapshot.zcode_running || snapshot.accounts.len() < 2 {
                exhausted_once = None;
                continue;
            }
            let Ok(active_quota) = store::live_quota(&paths) else {
                exhausted_once = None;
                continue;
            };
            if quota::conversation_availability(&active_quota)
                != quota::ConversationAvailability::Exhausted
            {
                exhausted_once = None;
                notified = None;
                continue;
            }
            if exhausted_once.as_deref() != Some(active_id.as_str()) {
                exhausted_once = Some(active_id.clone());
                continue;
            }

            let Some(active_index) = snapshot.accounts.iter().position(|a| a.id == active_id)
            else {
                exhausted_once = None;
                continue;
            };
            let mut target_id = None;
            for offset in 1..snapshot.accounts.len() {
                let candidate =
                    &snapshot.accounts[(active_index + offset) % snapshot.accounts.len()];
                if !candidate.has_config {
                    continue;
                }
                let Ok(candidate_quota) = store::account_quota(&paths, &candidate.id) else {
                    continue;
                };
                if quota::conversation_availability(&candidate_quota)
                    == quota::ConversationAvailability::Available
                {
                    target_id = Some(candidate.id.clone());
                    break;
                }
            }
            let Some(target_id) = target_id else {
                if notified.as_deref() != Some(active_id.as_str()) {
                    if let Some(h) = global_hub() {
                        h.emit("auto-switch-result", &json!({ "status": "no-candidate" }));
                    }
                    notified = Some(active_id);
                }
                continue;
            };

            // 网络调用后复核，防止过期监控轮覆盖手动切换或开关关闭。
            let result = {
                let _guard = store_guard();
                match store::get_state(&paths) {
                    Ok(now)
                        if load_settings(&paths).auto_switch()
                            && now.zcode_running
                            && now.active_account_id.as_deref() == Some(active_id.as_str()) =>
                    {
                        Some(store::switch_to(&paths, &target_id, true, true, false))
                    }
                    _ => None,
                }
            };
            match result {
                Some(Ok(switched)) => {
                    exhausted_once = None;
                    notified = None;
                    if let Some(h) = global_hub() {
                        h.emit("state-changed", &Value::Null);
                        if switched.switched {
                            h.emit(
                                "auto-switch-result",
                                &json!({
                                    "status": "switched",
                                    "name": switched.name,
                                    "launchError": switched.launch_error,
                                    "configStale": switched.config_stale,
                                }),
                            );
                        }
                    }
                }
                Some(Err(error)) => {
                    eprintln!("auto switch failed: {error}");
                    if notified.as_deref() != Some(active_id.as_str()) {
                        if let Some(h) = global_hub() {
                            h.emit(
                                "auto-switch-result",
                                &json!({ "status": "error", "error": error }),
                            );
                        }
                        notified = Some(active_id);
                    }
                    exhausted_once = None;
                }
                None => {
                    exhausted_once = None;
                    notified = None;
                }
            }
        }
    });
}
