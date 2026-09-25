// Z-Accounts 双模式桥接层（桌面 Tauri / Termux Web 单源共用）
//
// 桌面构建（vite + tauri）：import 此模块时 window.__TAURI_INTERNALS__ 已由
// Tauri 注入，动态 import 实际的 @tauri-apps/api（vite 会打包该分支）。
//
// Web 构建（无打包、浏览器原生 ESM 直服）：__TAURI_INTERNALS__ 不存在，
// 永不触碰 bare import，改走 HTTP POST /api/<cmd> + SSE /api/events。
//
// 接口签名与 @tauri-apps/api 对齐：
//  - invoke(cmd, args) → Promise<any>；失败时以「原始错误字符串」reject（同桌面）
//  - listen(event, handler) → Promise<unlisten>；handler 收到 { event, payload }
//  - emit(event, payload?) → Promise<void>

const IS_TAURI = typeof window.__TAURI_INTERNALS__ !== "undefined";

// 桌面模式下动态加载原生 API（web 模式永不触碰 bare import）。
// 不用顶层 await：rollup 输出 esm 对 top-level await 兼容性不稳，且这里
// 三处入口都会先 await tauriReady，时序无空窗。
let tauriCore = null;
let tauriEvent = null;
let tauriReady = null;
if (IS_TAURI) {
  tauriReady = (async () => {
    tauriCore = await import("@tauri-apps/api/core");
    tauriEvent = await import("@tauri-apps/api/event");
  })();
}

// ---------------- Web 模式实现 ----------------

let sse = null;
const listeners = new Map(); // event -> Set<handler>

function ensureSse() {
  if (sse) return sse;
  sse = new EventSource("/api/events");
  sse.onmessage = (raw) => {
    let msg = null;
    try { msg = JSON.parse(raw.data); } catch { return; }
    const set = listeners.get(msg.event);
    if (!set) return;
    for (const fn of set) {
      try { fn({ event: msg.event, payload: msg.payload, id: 0 }); }
      catch (e) { console.warn("[bridge] listener error:", msg.event, e); }
    }
  };
  return sse;
}

async function webInvoke(cmd, args) {
  let resp;
  try {
    resp = await fetch(`/api/${cmd}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(args || {}),
    });
  } catch (e) {
    throw "err.network: " + (e && e.message ? e.message : String(e));
  }
  let body = null;
  try { body = await resp.json(); } catch { /* 空体 */ }
  if (body && typeof body === "object" && "ok" in body) {
    if (body.ok) return body.data ?? null;
    throw String(body.error ?? `err.http_${resp.status}`);
  }
  if (!resp.ok) throw `err.http_${resp.status}: ${resp.status}`;
  return body ?? null;
}

// ---------------- 统一导出 ----------------

export async function invoke(cmd, args) {
  if (IS_TAURI) { await tauriReady; return tauriCore.invoke(cmd, args); }
  return webInvoke(cmd, args);
}

export async function listen(event, handler) {
  if (IS_TAURI) { await tauriReady; return tauriEvent.listen(event, handler); }
  ensureSse();
  let set = listeners.get(event);
  if (!set) { set = new Set(); listeners.set(event, set); }
  set.add(handler);
  return () => { set.delete(handler); };
}

export async function emit(event, payload) {
  if (IS_TAURI) { await tauriReady; return tauriEvent.emit(event, payload); }
  return webInvoke("emit", { event, payload: payload ?? null });
}

// ---------------- Web 模式专用辅助 ----------------

/** 是否 Web/Termux 模式（前端据此隐藏桌面专属 UI、替换打开设置页等行为） */
export const IS_WEB = !IS_TAURI;

/**
 * Web 模式下打开 captcha 验证页的页面内浮层。
 * 桌面版由后端弹独立小窗（auto=true 时隐藏运行）；Web 用 iframe 等价：
 *  - 手动领取：居中模态浮层，用户可看到验证码弹窗
 *  - 自动领取(auto=true)：1px 离屏 iframe，承载无感验证（若风控转人工，
 *    captcha.js 会 emit captcha://interactive → 主页面取消自动领取）
 * 返回关闭函数。
 */
export function openCaptchaOverlay(auto) {
  if (!IS_WEB) return () => {};
  document.querySelector(".cap-overlay")?.remove();
  const mask = document.createElement("div");
  mask.className = "cap-overlay" + (auto ? " auto" : "");
  mask.innerHTML = `
    <div class="cap-overlay-panel">
      <div class="cap-overlay-title"></div>
      <iframe src="/captcha.html" title="captcha"></iframe>
    </div>`;
  document.body.appendChild(mask);
  mask.addEventListener("click", (e) => { if (e.target === mask) closeCaptchaOverlay(); });
  return closeCaptchaOverlay;
}

export function closeCaptchaOverlay() {
  document.querySelector(".cap-overlay")?.remove();
}

/**
 * Web 模式下读取本地 .zsb 文件（替代桌面版系统文件对话框），
 * 返回 {name, content} 数组供 import_pick_files 上传。
 */
export function pickLocalFiles() {
  return new Promise((resolve, reject) => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".zsb,application/json";
    input.multiple = true;
    input.onchange = async () => {
      const files = [...input.files || []];
      if (!files.length) return resolve([]);
      try {
        const out = [];
        for (const f of files) out.push({ name: f.name, content: await f.text() });
        resolve(out);
      } catch (e) { reject(e); }
    };
    input.click();
  });
}
