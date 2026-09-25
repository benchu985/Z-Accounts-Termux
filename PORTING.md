# 移植说明（Termux Web 版）

本仓库基于上游 [Kang-code-sudo/Z-Accounts](https://github.com/Kang-code-sudo/Z-Accounts) 的完整 fork。
**业务核心零复制**：`web/` crate 通过 `#[path]` 直接 include 上游 `src-tauri/src/` 的纯逻辑模块，
上游更新时只需合并 + 保持下述 4 处补丁即可同步全部业务修复。

## 上游补丁清单（唯一需要随合并维护的差异）

| 文件 | 补丁 | 原因 |
|---|---|---|
| `src-tauri/src/oauth.rs` | `tauri::Url` → `url::Url`（2 处） | 脱离 tauri 框架也能编译；tauri::Url 本就是 url crate 的 re-export，桌面构建行为不变 |
| `src-tauri/src/store.rs` | `open_url()` 增加 `target_os = "android"` 分支调 `termux-open-url` | Android 无 xdg-open |
| `src-tauri/src/i18n.rs` | `resolve()` 在 Android 下默认中文（不用 sys-locale/LANG 推断） | sys-locale 的 Android 实现走 JNI，纯 Rust 进程无 JVM 不可用；且 Termux 的 LANG 默认多为 en_US.UTF-8，不能反映用户偏好 —— 无显式设置时默认中文，设置页可切换并持久化 |
| `src-tauri/Cargo.toml` + `web/Cargo.toml` | 增加 `url = "2"`；`sys-locale` 移入 `cfg(not(target_os="android"))` | 配合上述两处 |

## 架构

```
浏览器（手机 Chrome 或局域网设备）
   │  GET  /            → index.html（原生 ESM，无需 vite 构建）
   │  GET  /src/*.js|css → 前端源码直服
   │  GET  /api/events  → SSE 事件流（对应桌面版 app.emit → listen）
   │  POST /api/<cmd>   → 37 个命令（对应桌面版 invoke，参数同为 camelCase）
   │  POST /api/emit    → 前端广播（对应桌面版 emit，如 captcha://interactive）
   ▼
zaccounts-web（web/src/main.rs）
   ├── httpsrv.rs  std::net 极简 HTTP 服务器（线程/连接，刻意不用 axum/tokio：省依赖树与编译资源）
   ├── events.rs   SSE 事件总线（桌面版 5 个事件 + 前端广播合并为单流）
   └── api.rs      命令分发层，命令体逐一移植自上游 lib.rs 的 #[tauri::command]
          │
          └── #[path] include 上游纯逻辑模块
              store.rs（账号库/切号） quota.rs（额度归一化） oauth.rs（登录流）
              claim.rs（领取） cipher.rs（.zsb 备份） zcrypto.rs（凭据 enc:v1）
              model_status.rs flowlog.rs i18n.rs
```

### 与桌面版的行为差异（全部有源可溯）

| 桌面版 | Web 版 |
|---|---|
| `app.emit` → 前端 `listen` | SSE `/api/events`，消息体 `{event, payload}` |
| OAuth 弹窗拦截 `zcode://` 深链 | 纯 poll 通道（上游 oauth.rs 本就实现了 init/poll 设备流）：`oauth_begin` 返回 `authorizeUrl`，前端新开标签页，后端轮询完成后经 SSE 推 `oauth://done` |
| 后端弹 captcha 独立小窗 | `claim_start` 后由前端挂 `/captcha.html` 的 iframe 浮层（手动领取居中显示；自动领取 1px 离屏承载无感验证） |
| 系统保存/打开对话框 | 导出直接写手机下载目录（`~/storage/downloads` 优先）；导入用浏览器 `<input type=file>` 读内容上传，sealed 校验仍在服务端 |
| 托盘 / 自启动 / close-to-tray / reveal_main / open_settings | 删除或空实现；设置页 Web 模式隐藏对应开关 |
| `claim_refresh` 前置的激活遥测（伪造 app_launch/app_daily_active 事件） | **已移除**：实测对 Start Plan 授予无效（服务端条件授予，不存在 claim/activate 端点，见逆向分析 docs/auth/activation-protocol.md），只带来隐私暴露与请求开销 |
| 错误经 invoke 以原始字符串 reject | 响应体 `{ok:false, error}`，bridge 以原始字符串 reject，保持 `^[a-z_]+:` 错误码契约（前端 stripErr 依赖） |

### Termux 环境注意

- `iana-time-zone` 在 Android 上读 `persist.sys.timezone` 系统属性（纯 Rust，无 JVM），可用。
- 二进制链接 bionic（NDK 交叉编译），DNS 走 Android netd，与 Termux 原生程序一致。
- **不要**用 glibc/musl 静态链接方案：Android 根文件系统无 `/etc/resolv.conf`，DNS 解析会失败。

## 构建（GitHub Actions 交叉编译，避免手机编译压力）

`.github/workflows/build-web.yml`：ubuntu runner + NDK + `cargo ndk -t arm64-v8a -p 24`，
产物 `zaccounts-web-aarch64`（静态目录还包括 `zaccounts-web-frontend` 备用）。

本地取产物：

```sh
gh run download --repo benchu985/Z-Accounts-Termux -n zaccounts-web-aarch64 -D ~/bin
chmod +x ~/bin/zaccounts-web
~/zaccounts-web/repo/start.sh   # 或仓库内 ./start.sh
```

## 已知边界（Termux 上无 ZCode 客户端所致）

`launch_zcode` / `kill_zcode` / `capture_current` / `update_account_from_live` /
`model_status` 依赖本机运行中的 ZCode 进程（官方仅有 Windows/macOS/Linux 桌面版）。
在 Termux 上这些操作会以错误提示优雅失败；切号（`switch_to`）仍会写入
`$ZCODE_DATA_BASE_DIR`（默认 `$HOME`）下 `.zcode/v2/` 的凭据文件——
若该目录与 PC 同步（如 Syncthing），Web 版即成为远程切号面板。

其余功能（账号库、额度仪表盘、自动领取、OAuth 添加账号、.zsb 加密导入导出、自动切换监控）完整可用。
