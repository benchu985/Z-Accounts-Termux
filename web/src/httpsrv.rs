//! 极简 HTTP/1.1 服务器：std::net + 每连接一个线程。
//! 本地单用户场景（浏览器 + SSE + 小 JSON POST）足够，避免 axum/tokio 的大依赖树。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

pub struct Request {
    pub method: String,
    /// 已解码的 URL 路径（不含查询串）
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn wants_keepalive(&self) -> bool {
        let conn = self.header("connection").unwrap_or("").to_ascii_lowercase();
        if conn.contains("close") {
            return false;
        }
        true // HTTP/1.1 默认 keep-alive
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub extra_headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response {
            status,
            content_type: content_type.to_string(),
            extra_headers: vec![],
            body,
        }
    }
    pub fn text(status: u16, body: &str) -> Response {
        Response::bytes(
            status,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }
    pub fn json_ok(v: serde_json::Value) -> Response {
        let body =
            serde_json::to_vec(&serde_json::json!({ "ok": true, "data": v })).unwrap_or_default();
        Response::bytes(200, "application/json", body)
    }
    pub fn json_err(e: &str) -> Response {
        let body =
            serde_json::to_vec(&serde_json::json!({ "ok": false, "error": e })).unwrap_or_default();
        Response::bytes(200, "application/json", body)
    }
    pub fn set_header(&mut self, name: &str, value: &str) {
        self.extra_headers
            .push((name.to_string(), value.to_string()));
    }
}

/// 路由结果：普通响应，或接管整条连接（SSE 长连接）。
pub enum Reply {
    Respond(Response),
    TakeOver(Box<dyn FnOnce(TcpStream) + Send>),
}

impl Reply {
    pub fn respond(r: Response) -> Reply {
        Reply::Respond(r)
    }
    pub fn take_over(f: Box<dyn FnOnce(TcpStream) + Send>) -> Reply {
        Reply::TakeOver(f)
    }
}

pub type Router = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

pub fn serve(listener: TcpListener, router: Router) -> ! {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let router = router.clone();
                std::thread::spawn(move || handle_connection(stream, router));
            }
            Err(e) => {
                eprintln!("accept error: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_connection(stream: TcpStream, router: Router) {
    stream.set_read_timeout(Some(Duration::from_secs(600))).ok();
    stream.set_nodelay(true).ok();
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return, // 对端关闭
            Err(_) => return,
        };
        let keep = req.wants_keepalive();
        match router(&req) {
            Reply::Respond(resp) => {
                if write_response(&mut writer, &resp, keep, req.method == "HEAD").is_err() {
                    return;
                }
                if !keep {
                    return;
                }
            }
            Reply::TakeOver(f) => {
                // 连接交给 SSE：从 BufReader 还原出原始流
                drop(writer); // 只丢弃克隆 fd，不 shutdown（会关掉共享 socket）
                let stream = reader.into_inner();
                f(stream);
                return;
            }
        }
    }
}

/// 读取一个完整请求头；连接刚建立即 EOF 时返回 Ok(None)。
fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            // 读超时（BufReader 内部已缓冲部分数据的情况极少；直接放弃）
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let line = line.trim_end();
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (decode_path(p), q.to_string()),
        None => (decode_path(&target), String::new()),
    };

    let mut headers: Vec<(String, String)> = vec![];
    let mut total = line.len();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        total += h.len();
        if total > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body too large",
        ));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn decode_path(p: &str) -> String {
    let bytes = p.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &p[i + 1..i + 3];
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn write_response(
    w: &mut TcpStream,
    resp: &Response,
    keep: bool,
    head_only: bool,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len(),
        if keep { "keep-alive" } else { "close" },
    );
    for (k, v) in &resp.extra_headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    if !head_only {
        w.write_all(&resp.body)?;
    }
    w.flush()
}
