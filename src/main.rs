mod state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequest, Multipart, Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use futures::StreamExt;
use rand::Rng;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::state::{AppState, Entry, MAX_FILE_SIZE, QUOTA_BYTES, TTL};

const ID_ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const ID_LEN: usize = 8;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dir =
        PathBuf::from(std::env::var("UPLOAD_DIR").unwrap_or_else(|_| "/tmp/justupload".into()));
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("create upload dir");
    // Start clean: nothing on disk outlives a restart.
    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }

    let st = Arc::new(AppState::new(dir, std::env::var("BASE_URL").ok()));

    tokio::spawn({
        let st = st.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                for e in st.expired() {
                    tracing::info!(name = %e.name, "expired");
                    let _ = tokio::fs::remove_file(&e.path).await;
                }
                st.gc_quota();
            }
        }
    });

    let app = Router::new()
        .route("/", any(root))
        .route("/health", get(health))
        .route("/:id", get(download))
        .route("/:id/:name", get(download_named))
        .layer(DefaultBodyLimit::max(MAX_FILE_SIZE + 64 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(st);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    tracing::info!(%addr, "listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .unwrap();
}

async fn health(State(st): State<Arc<AppState>>) -> String {
    let (n, bytes) = st.stats();
    format!("ok\nfiles={n}\nbytes={bytes}\n")
}

async fn root(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    match *req.method() {
        Method::GET | Method::HEAD => index(&st, req.headers()),
        Method::POST | Method::PUT => upload(st, peer, req).await,
        _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n").into_response(),
    }
}

fn base(st: &AppState, headers: &HeaderMap) -> String {
    if let Some(b) = &st.base_url {
        return b.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or(
            if host.starts_with("localhost") || host.starts_with("127.") {
                "http"
            } else {
                "https"
            },
        );
    format!("{scheme}://{host}")
}

fn index(st: &AppState, headers: &HeaderMap) -> Response {
    let b = base(st, headers);
    let html = headers
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .map(|a| a.contains("text/html"))
        .unwrap_or(false);
    if html {
        let page = include_str!("index.html").replace("{{BASE}}", &b);
        return ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], page).into_response();
    }
    let txt = format!(
        "justupload - temporary file sharing\n\
         \n\
         upload:\n\
         \x20 curl -T file.txt {b}/\n\
         \x20 curl -F 'file=@file.txt' {b}/\n\
         \x20 wget --method=PUT --body-file=file.txt -qO- {b}/\n\
         \n\
         download:\n\
         \x20 curl -OJ <returned url>\n\
         \n\
         rules:\n\
         \x20 - max file size: 10 MB\n\
         \x20 - deleted after the first download\n\
         \x20 - deleted after 1 hour\n\
         \x20 - max 30 MB of uploads per hour per IP\n\
         \x20 - no backups, no guarantees\n"
    );
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], txt).into_response()
}

fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    for h in ["fly-client-ip", "x-real-ip"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            return v.to_string();
        }
    }
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            return first.trim().to_string();
        }
    }
    peer.ip().to_string()
}

fn gen_id() -> String {
    let mut rng = rand::thread_rng();
    (0..ID_LEN)
        .map(|_| ID_ALPHABET[rng.gen_range(0..ID_ALPHABET.len())] as char)
        .collect()
}

/// Keep only a safe, printable basename; fall back to "file".
fn sanitize(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(96)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

async fn upload(st: Arc<AppState>, peer: SocketAddr, req: Request) -> Response {
    let headers = req.headers().clone();
    let ip = client_ip(&headers, peer);
    let remaining = st.remaining(&ip);
    if remaining == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "quota exceeded: {} MB per hour per IP, try again later\n",
                QUOTA_BYTES / 1024 / 1024
            ),
        )
            .into_response();
    }
    let cap = remaining.min(MAX_FILE_SIZE as u64);

    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("multipart/form-data"))
        .unwrap_or(false);

    let id = gen_id();
    let path = st.dir.join(&id);

    let result = if is_multipart {
        match Multipart::from_request(req, &()).await {
            Ok(mp) => write_multipart(mp, &path, cap).await,
            Err(e) => Err(UploadError::Message(e.to_string())),
        }
    } else {
        let hinted = headers
            .get("x-filename")
            .and_then(|v| v.to_str().ok())
            .map(sanitize);
        write_raw(req.into_body(), &path, cap)
            .await
            .map(|size| (hinted, size))
    };

    let (name, size) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            return e.into_response(cap);
        }
    };

    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return (StatusCode::BAD_REQUEST, "empty upload\n").into_response();
    }

    if st.try_reserve(&ip, size).is_err() {
        let _ = tokio::fs::remove_file(&path).await;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "quota exceeded, try again later\n",
        )
            .into_response();
    }

    let name = name.unwrap_or_else(|| "file".to_string());
    st.insert(
        id.clone(),
        Entry {
            name: name.clone(),
            path,
            size,
            created: Instant::now(),
        },
    );
    tracing::info!(%id, %name, size, "uploaded");

    let url = format!("{}/{}/{}", base(&st, &headers), id, name);
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{url}\n"),
    )
        .into_response()
}

enum UploadError {
    TooLarge,
    Io,
    Message(String),
    NoFile,
}

impl UploadError {
    fn into_response(self, cap: u64) -> Response {
        match self {
            UploadError::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("too large: at most {cap} bytes accepted right now (10 MB per file, 30 MB per hour per IP)\n"),
            )
                .into_response(),
            UploadError::Io => {
                (StatusCode::INTERNAL_SERVER_ERROR, "storage error\n".to_string()).into_response()
            }
            UploadError::Message(m) => (StatusCode::BAD_REQUEST, format!("{m}\n")).into_response(),
            UploadError::NoFile => (
                StatusCode::BAD_REQUEST,
                "no file in multipart body (use -F 'file=@path')\n".to_string(),
            )
                .into_response(),
        }
    }
}

async fn write_raw(body: Body, path: &std::path::Path, cap: u64) -> Result<u64, UploadError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|_| UploadError::Io)?;
    let mut stream = body.into_data_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| UploadError::Message(e.to_string()))?;
        total += chunk.len() as u64;
        if total > cap {
            return Err(UploadError::TooLarge);
        }
        file.write_all(&chunk).await.map_err(|_| UploadError::Io)?;
    }
    file.flush().await.map_err(|_| UploadError::Io)?;
    Ok(total)
}

async fn write_multipart(
    mut mp: Multipart,
    path: &std::path::Path,
    cap: u64,
) -> Result<(Option<String>, u64), UploadError> {
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| UploadError::Message(e.to_string()))?
    {
        let name = field.file_name().map(sanitize);
        if name.is_none() && field.name() != Some("file") {
            continue;
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|_| UploadError::Io)?;
        let mut total: u64 = 0;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| UploadError::Message(e.to_string()))?
        {
            total += chunk.len() as u64;
            if total > cap {
                return Err(UploadError::TooLarge);
            }
            file.write_all(&chunk).await.map_err(|_| UploadError::Io)?;
        }
        file.flush().await.map_err(|_| UploadError::Io)?;
        return Ok((name, total));
    }
    Err(UploadError::NoFile)
}

async fn download_named(
    State(st): State<Arc<AppState>>,
    Path((id, _name)): Path<(String, String)>,
) -> Response {
    serve(st, id).await
}

async fn download(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    serve(st, id).await
}

async fn serve(st: Arc<AppState>, id: String) -> Response {
    let Some(entry) = st.take(&id) else {
        return (
            StatusCode::NOT_FOUND,
            "not found (already downloaded or expired)\n",
        )
            .into_response();
    };
    if Instant::now().duration_since(entry.created) >= TTL {
        let _ = tokio::fs::remove_file(&entry.path).await;
        return (StatusCode::NOT_FOUND, "expired\n").into_response();
    }
    let file = match tokio::fs::File::open(&entry.path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
    };
    // Unlink now: the open handle keeps the bytes alive until the response is finished.
    let _ = tokio::fs::remove_file(&entry.path).await;
    tracing::info!(%id, name = %entry.name, "downloaded");

    let mime = mime_guess::from_path(&entry.name)
        .first_raw()
        .unwrap_or("application/octet-stream");
    (
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CONTENT_LENGTH, entry.size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", entry.name),
            ),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response()
}
