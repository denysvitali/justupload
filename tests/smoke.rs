use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(port: u16, dir: &str) -> Server {
    let bin = env!("CARGO_BIN_EXE_justupload");
    let child = Command::new(bin)
        .env("PORT", port.to_string())
        .env("UPLOAD_DIR", dir)
        .env("BASE_URL", format!("http://127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Server(child)
}

/// Minimal HTTP/1.1 client: returns (status line + headers, body).
fn request(port: u16, head: &str, body: &[u8]) -> (String, Vec<u8>) {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    s.flush().unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("headers");
    (
        String::from_utf8_lossy(&raw[..split]).to_string(),
        raw[split + 4..].to_vec(),
    )
}

fn put(port: u16, payload: &[u8], filename: &str) -> (String, Vec<u8>) {
    let head = format!(
        "PUT / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Filename: {filename}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    request(port, &head, payload)
}

fn get(port: u16, path: &str) -> (String, Vec<u8>) {
    let head =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    request(port, &head, b"")
}

fn tmpdir(tag: &str) -> String {
    let d = std::env::temp_dir().join(format!("justupload-test-{tag}"));
    let _ = std::fs::remove_dir_all(&d);
    d.to_string_lossy().to_string()
}

#[test]
fn upload_then_download_once() {
    let port = 18081;
    let _srv = start(port, &tmpdir("once"));

    let (h, b) = put(port, b"hello world", "greeting.txt");
    assert!(h.starts_with("HTTP/1.1 201"), "{h}");
    let url = String::from_utf8(b).unwrap().trim().to_string();
    assert!(url.ends_with("/greeting.txt"), "{url}");

    let path = url.trim_start_matches(&format!("http://127.0.0.1:{port}"));
    let (h, b) = get(port, path);
    assert!(h.starts_with("HTTP/1.1 200"), "{h}");
    assert!(h.contains("greeting.txt"), "{h}");
    assert_eq!(b, b"hello world");

    // Second download must fail: one download only.
    let (h, _) = get(port, path);
    assert!(h.starts_with("HTTP/1.1 404"), "{h}");
}

/// `curl -T file.txt https://host/` sends `PUT /file.txt`, not `PUT /`.
#[test]
fn curl_upload_file_puts_to_named_path() {
    let port = 18087;
    let _srv = start(port, &tmpdir("curl-t"));
    let payload = b"binary-ish\x00\x01contents";
    let head = format!(
        "PUT /provisioning-api HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let (h, b) = request(port, &head, payload);
    assert!(h.starts_with("HTTP/1.1 201"), "{h}");
    let url = String::from_utf8(b).unwrap().trim().to_string();
    assert!(url.ends_with("/provisioning-api"), "{url}");

    let path = url.trim_start_matches(&format!("http://127.0.0.1:{port}"));
    let (h, b) = get(port, path);
    assert!(h.starts_with("HTTP/1.1 200"), "{h}");
    assert_eq!(b, payload);
}

#[test]
fn rejects_oversized_file() {
    let port = 18082;
    let _srv = start(port, &tmpdir("big"));
    let payload = vec![b'x'; 11 * 1024 * 1024];
    let (h, _) = put(port, &payload, "big.bin");
    assert!(
        h.starts_with("HTTP/1.1 413") || h.starts_with("HTTP/1.1 400"),
        "{h}"
    );
}

#[test]
fn enforces_hourly_quota() {
    let port = 18083;
    let _srv = start(port, &tmpdir("quota"));
    let payload = vec![b'x'; 8 * 1024 * 1024];
    // 4 x 8 MB = 32 MB > 30 MB budget, so the last one must be rejected.
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let (h, _) = put(port, &payload, "chunk.bin");
        statuses.push(h.lines().next().unwrap().to_string());
    }
    assert!(statuses[0].contains("201"), "{statuses:?}");
    assert!(
        statuses.last().unwrap().contains("429") || statuses.last().unwrap().contains("413"),
        "{statuses:?}"
    );
}

#[test]
fn index_is_plain_text_for_curl() {
    let port = 18084;
    let _srv = start(port, &tmpdir("index"));
    let (h, b) = get(port, "/");
    assert!(h.starts_with("HTTP/1.1 200"), "{h}");
    assert!(h.contains("text/plain"), "{h}");
    assert!(String::from_utf8_lossy(&b).contains("curl -T"));
}

#[test]
fn unknown_id_is_404() {
    let port = 18085;
    let _srv = start(port, &tmpdir("404"));
    let (h, _) = get(port, "/doesnotexist/x.txt");
    assert!(h.starts_with("HTTP/1.1 404"), "{h}");
}

#[test]
fn multipart_upload_keeps_filename() {
    let port = 18086;
    let _srv = start(port, &tmpdir("mp"));
    let boundary = "----justuploadtest";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.md\"\r\nContent-Type: text/markdown\r\n\r\n# hi\r\n--{boundary}--\r\n"
    );
    let head = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let (h, b) = request(port, &head, body.as_bytes());
    assert!(h.starts_with("HTTP/1.1 201"), "{h}");
    let url = String::from_utf8(b).unwrap().trim().to_string();
    assert!(url.ends_with("/notes.md"), "{url}");

    let path = url.trim_start_matches(&format!("http://127.0.0.1:{port}"));
    let (_, b) = get(port, path);
    assert_eq!(b, b"# hi");
}
