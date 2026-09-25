#!/data/data/com.termux/files/usr/bin/sh
# Z-Accounts Web 版启动脚本（Termux）
#
# 用法:
#   ./start.sh                    # 本机访问  http://127.0.0.1:8790
#   HOST=0.0.0.0 ./start.sh       # 允许局域网设备访问（注意：账号凭据工具，仅在可信网络开放）
#   PORT=9000 ./start.sh          # 自定义端口
#
# 二进制定位顺序:
#   1) 本仓库 web/target/.../zaccounts-web （本地 cargo build 产物）
#   2) $HOME/bin/zaccounts-web            （GitHub Actions 产物安装位置）

set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"

BIN="$ROOT/web/target/aarch64-linux-android/release/zaccounts-web"
[ -x "$BIN" ] || BIN="$HOME/bin/zaccounts-web"
if [ ! -x "$BIN" ]; then
  echo "未找到 zaccounts-web 二进制。"
  echo "安装 GitHub Actions 产物: gh run download --repo benchu985/Z-Accounts-Termux -n zaccounts-web-aarch64 -D \"$HOME/bin\" && mv \"$HOME/bin/zaccounts-web\" \"$HOME/bin/zaccounts-web\""
  exit 1
fi

exec "$BIN" --root "$ROOT" --port "${PORT:-8790}" --host "${HOST:-127.0.0.1}"
