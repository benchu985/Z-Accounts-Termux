//! 事件总线 + SSE 通道：对应桌面版 Tauri 的 `app.emit` / 前端 `listen`。
//!
//! 桌面版有 5 个后端→前端事件（tray-action / claim://result / oauth://done /
//! state-changed / auto-switch-result）+ 1 个前端广播（captcha://interactive）。
//! Web 版全部合并到同一条 `/api/events` SSE 流，消息体为 {event, payload} JSON。

use serde_json::{Map, Value};
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Hub {
    /// 每个活跃 SSE 连接一个 sender；发送失败即移除该连接。
    subs: Mutex<Vec<(u64, Sender<String>)>>,
}

impl Hub {
    pub fn new() -> Hub {
        Hub {
            subs: Mutex::new(Vec::new()),
        }
    }

    /// 广播一条事件给所有已连接页面（等价于桌面版的 app.emit）。
    pub fn emit(&self, event: &str, payload: &Value) {
        let mut obj = Map::new();
        obj.insert("event".to_string(), Value::String(event.to_string()));
        obj.insert("payload".to_string(), payload.clone());
        let frame = match serde_json::to_string(&Value::Object(obj)) {
            Ok(s) => s,
            Err(_) => return,
        };
        let sse = format!("data: {frame}\n\n");
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.retain(|(_, tx)| tx.send(sse.clone()).is_ok());
    }

    fn subscribe(&self) -> (u64, Receiver<String>) {
        let (tx, rx) = channel::<String>();
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        let id = subs.iter().map(|(i, _)| *i).max().unwrap_or(0) + 1;
        subs.push((id, tx));
        (id, rx)
    }

    fn unsubscribe(&self, id: u64) {
        self.subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(i, _)| *i != id);
    }
}

/// 打开一条 SSE 连接并保持到客户端断开。
///
/// 连接生命周期：
///  - 订阅后立即丢掉本地 sender 副本，令 subs 中那份成为唯一 sender；
///    这样对端断开 → 读线程清理订阅（sender drop）→ rx.recv() 返回 Err → 写循环退出。
///  - 空闲期每 20 秒发送 `: ping` 注释帧，防止中间层静默断连。
pub fn run_sse(stream: TcpStream, hub: &Arc<Hub>) {
    let (id, rx) = hub.subscribe();

    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => {
            hub.unsubscribe(id);
            return;
        }
    };
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n";
    if w.write_all(head.as_bytes()).is_err() || w.flush().is_err() {
        hub.unsubscribe(id);
        return;
    }

    // 读线程：阻塞等对端关闭（浏览器 tab 关闭/网络断开即结束），负责清理订阅。
    {
        let hub2 = hub.clone();
        let mut s = stream;
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match std::io::Read::read(&mut s, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = s.shutdown(Shutdown::Both);
            hub2.unsubscribe(id);
        });
    }

    let ping = b": ping\n\n";
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(msg) => {
                if w.write_all(msg.as_bytes()).is_err() {
                    break;
                }
                if w.flush().is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if w.write_all(ping).is_err() || w.flush().is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    hub.unsubscribe(id);
}
