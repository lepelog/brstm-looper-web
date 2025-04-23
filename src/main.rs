use std::env;
use std::fs::{create_dir, read_dir};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{
    Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use brstm::encoder::EncodingError;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::signal;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::timeout;
use tower_http::cors::CorsLayer;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use tracing::{error, info};
use util::DeleteOnDrop;

mod ffmpeg;
mod util;

#[derive(Debug, Clone)]
struct AppState {
    temp_dir: Arc<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    // initialize tracing
    tracing_subscriber::fmt::init();

    ffmpeg_next::init().expect("could not init ffmpeg!");

    let temp_dir = std::path::PathBuf::from(
        env::var("LOOPER_TEMP_DIR").expect("LOOPER_TEMP_DIR needs to be specified!"),
    );

    if !temp_dir.exists() {
        create_dir(&temp_dir).expect("could not create temp dir!")
    }

    if !temp_dir.is_dir() {
        panic!("{} is not a directory!", temp_dir.display());
    }

    // for restarts, find highest id in temp directory
    let max_cur_id = read_dir(&temp_dir)
        .expect("read dir failed")
        .filter_map(|entry| {
            entry.ok().and_then(|e| {
                e.file_name().to_str().and_then(|s| {
                    s.strip_prefix(UPLOAD_PREFIX)
                        .and_then(|s| s.parse::<usize>().ok())
                })
            })
        })
        .max();

    CURRENT_ID.store(
        max_cur_id.unwrap_or_default() + 1,
        std::sync::atomic::Ordering::Relaxed,
    );

    let temp_dir = Arc::new(temp_dir);

    {
        let temp_dir = Arc::clone(&temp_dir);
        // periodically run cleanup
        tokio::task::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60 * 60 * 7)).await;
            info!("running periodic cleanup");
            filelimit_cleanup(Arc::clone(&temp_dir)).await;
        });
    }

    // build our application with a route
    let app = Router::new()
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .route(
            "/encode-brstm/{id}/{filename}",
            get(encode_brstm).layer(
                CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods(tower_http::cors::Any),
            ),
        )
        .route(
            "/analyze-loops",
            post(analyze_loops).layer(
                CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods(tower_http::cors::Any),
            ),
        )
        .route("/", get(index))
        .route("/health", get(health))
        .with_state(AppState { temp_dir });

    // run our app with hyper, listening globally on PORT or 3000
    let listener = tokio::net::TcpListener::bind(format!(
        "0.0.0.0:{}",
        std::env::var("PORT").unwrap_or("3000".into())
    ))
    .await
    .unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn index() -> Response {
    let content = include_str!("../web/index.html");
    Response::builder()
        .header("Content-Type", "text/html")
        .body(Body::from(content))
        .unwrap()
}

fn make_path_from_id(temp_dir: &std::path::Path, id: usize) -> std::path::PathBuf {
    temp_dir.join(format!("{UPLOAD_PREFIX}{id}"))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Loop {
    pub start: f64,
    pub end: f64,
    pub note_distance: f32,
    pub loudness_difference: f32,
    pub score: f32,
}

impl Loop {
    pub fn parse<'a>(mut elems: impl Iterator<Item = &'a str>) -> Option<Self> {
        Some(Loop {
            start: elems.next()?.parse().ok()?,
            end: elems.next()?.parse().ok()?,
            note_distance: elems.next()?.parse().ok()?,
            loudness_difference: elems.next()?.parse().ok()?,
            score: elems.next()?.parse().ok()?,
        })
    }
}

#[derive(Debug)]
enum UploadError {
    TooBig(u64),
    Internal,
    Timeout,
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        match self {
            UploadError::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            UploadError::TooBig(size) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Send Size ({size} bytes) exceeds limit of 15MiB!"),
            )
                .into_response(),
            UploadError::Timeout => StatusCode::REQUEST_TIMEOUT.into_response(),
        }
    }
}

#[derive(Debug)]
enum EncoderError {
    FfmpegError {
        msg: &'static str,
        err: Option<ffmpeg_next::Error>,
        user_fault: bool,
    },
    UploadError(UploadError),
    EncodingError(EncodingError),
    Custom(String),
}

impl From<UploadError> for EncoderError {
    fn from(value: UploadError) -> Self {
        EncoderError::UploadError(value)
    }
}

trait FfmpegErrorExt<T> {
    fn context(self, msg: &'static str, user_fault: bool) -> Result<T, EncoderError>;
}

impl<T> FfmpegErrorExt<T> for Result<T, ffmpeg_next::Error> {
    fn context(self, msg: &'static str, user_fault: bool) -> Result<T, EncoderError> {
        self.map_err(|err| EncoderError::FfmpegError {
            msg,
            err: Some(err),
            user_fault,
        })
    }
}

impl IntoResponse for EncoderError {
    fn into_response(self) -> Response {
        match self {
            EncoderError::FfmpegError {
                msg,
                err,
                user_fault,
            } => {
                error!("{self:?}");
                (
                    if user_fault {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    },
                    if let Some(err) = err {
                        format!("{msg}: {err}")
                    } else {
                        msg.to_string()
                    },
                )
                    .into_response()
            }
            EncoderError::UploadError(e) => e.into_response(),
            EncoderError::EncodingError(e) => {
                (StatusCode::BAD_REQUEST, format!("encode failed: {e}")).into_response()
            }
            EncoderError::Custom(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    }
}

const UPLOAD_LIMIT: u64 = 15 * 1024 * 1024; // 15MiB
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_PREFIX: &str = "looper-temp-";
const MAX_TOTAL_UPLOAD: u64 = 2 * 1024 * 1024 * 1024; // 2GiB
static ANALYZE_LIMIT: Semaphore = Semaphore::const_new(10);
// there is no way we run out of them
static CURRENT_ID: AtomicUsize = AtomicUsize::new(0);
static CLEANUP_MUTEX: Mutex<()> = Mutex::const_new(());

async fn filelimit_cleanup(temp_dir: Arc<std::path::PathBuf>) {
    let Ok(_lock) = CLEANUP_MUTEX.try_lock() else {
        return;
    };
    async fn do_remove_file(filename: &std::path::Path, size: u64) {
        match tokio::fs::remove_file(filename).await {
            Ok(()) => info!(
                "successfully removed {}, reclaiming {} bytes",
                filename.display(),
                size
            ),
            // this should really not happen
            Err(e) => error!("error removing {}: {}", filename.display(), e),
        }
    }
    // get files with filesize
    let mut files = Vec::new();
    let Ok(readdir) = read_dir(temp_dir.as_path()) else {
        return;
    };
    for entry in readdir {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        // remove files older than a day
        if meta.accessed().is_ok_and(|a| {
            a.elapsed()
                .is_ok_and(|e| e > Duration::from_secs(60 * 60 * 24))
        }) {
            do_remove_file(&entry.path(), meta.len()).await;
            continue;
        }
        let Some(id): Option<usize> = entry
            .file_name()
            .to_str()
            .and_then(|s| s.strip_prefix(UPLOAD_PREFIX))
            .and_then(|s| s.parse().ok())
        else {
            continue;
        };

        files.push((id, meta.len()));
    }
    files.sort_unstable();
    while files.iter().map(|(_, len)| *len).sum::<u64>() > MAX_TOTAL_UPLOAD {
        let (id, size) = files.remove(0);
        let filename = make_path_from_id(temp_dir.as_path(), id);
        do_remove_file(&filename, size).await;
    }
}

#[derive(Debug, Serialize)]
struct AnalyzeResult {
    loops: Vec<Loop>,
    id: usize,
}

async fn analyze_loops(
    State(AppState { temp_dir }): State<AppState>,
    request: Request,
) -> Result<Json<AnalyzeResult>, UploadError> {
    let body = request.into_body();
    let size_hint = body.size_hint();
    if let Some(upper) = size_hint.upper() {
        if upper > UPLOAD_LIMIT {
            return Err(UploadError::TooBig(upper));
        }
    }
    if size_hint.lower() > UPLOAD_LIMIT {
        return Err(UploadError::TooBig(size_hint.lower()));
    }
    // the semaphore is never closed so it always succeeds
    let semaphore_timer = Instant::now();
    let _analyzer_permit = ANALYZE_LIMIT.acquire().await.unwrap();
    info!(
        "time to aquire semaphore permit: {}s",
        semaphore_timer.elapsed().as_secs_f64()
    );

    // maybe clean up old files in the background
    tokio::task::spawn(filelimit_cleanup(temp_dir.clone()));

    let current_id = CURRENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = make_path_from_id(temp_dir.as_path(), current_id);
    let mut out_f = tokio::fs::File::create(&temp_path).await.map_err(|e| {
        error!("could not create {}: {e}", temp_path.display());
        UploadError::Internal
    })?;

    let mut delete_temp_file = DeleteOnDrop::new(&temp_path);

    let receiver_fut = {
        let temp_path = temp_path.clone();
        async move {
            let receive_timer = Instant::now();

            let stream = body.into_data_stream();
            tokio::pin!(stream);

            let mut total_length: u64 = 0;
            loop {
                match stream.next().await {
                    Some(Ok(data)) => {
                        total_length += data.len() as u64;
                        if total_length > UPLOAD_LIMIT {
                            return Err(UploadError::TooBig(total_length));
                        }
                        out_f.write_all(&data).await.map_err(|e| {
                            error!("could not write to {}:{e}", temp_path.display());
                            UploadError::Internal
                        })?;
                    }
                    Some(Err(e)) => {
                        error!("could not read stream: {e}");
                        return Err(UploadError::Internal);
                    }
                    None => break,
                }
            }
            let _ = out_f.flush().await;
            drop(out_f);

            info!(
                "time to receive upload ({} bytes): {}s",
                total_length,
                receive_timer.elapsed().as_secs_f32()
            );
            Ok(total_length)
        }
    };
    let total_length = timeout(UPLOAD_TIMEOUT, receiver_fut)
        .await
        .map_err(|_| UploadError::Timeout)??;

    let analyze_timer = Instant::now();

    let output = Command::new("pymusiclooper")
        .arg("export-points")
        .arg("--path")
        .arg(&temp_path)
        .arg("--alt-export-top")
        .arg("-1")
        .arg("--fmt")
        .arg("SECONDS")
        .output()
        .await
        .map_err(|e| {
            error!("pymusiclooper spawn error: {e}");
            UploadError::Internal
        })?;

    // output form: newline, then, space separated: start end noteDistance loudnessDifference score
    let out = String::from_utf8(output.stdout).map_err(|e| {
        error!("pymusiclooper output error: {e}");
        UploadError::Internal
    })?;

    if !output.status.success() {
        error!("pymusiclooper error: {}", out);
        // should be more specific for garbage user data
        return Err(UploadError::Internal);
    }

    let loops = out
        .trim()
        .split('\n')
        .map(|line| {
            let elems = line.split(' ');
            Loop::parse(elems).ok_or_else(|| {
                error!("failed parsing pymusiclooper line {line}");
                UploadError::Internal
            })
        })
        .collect::<Result<Vec<Loop>, UploadError>>()?;

    info!(
        "time to analyze ({} bytes): {}s",
        total_length,
        analyze_timer.elapsed().as_secs_f32()
    );

    delete_temp_file.keep();

    Ok(Json(AnalyzeResult {
        loops,
        id: current_id,
    }))
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EncodeQuery {
    loop_point: Option<u32>,
    end: Option<usize>,
}

async fn encode_brstm(
    Query(options): Query<EncodeQuery>,
    Path((id_str, filename)): Path<(String, String)>,
    State(AppState { temp_dir }): State<AppState>,
) -> Result<Response, EncoderError> {
    info!("downloading {id_str} as {filename} with {options:?}");

    let cur_filename = make_path_from_id(temp_dir.as_path(), id_str.parse().unwrap()); // TODO

    let decode_timer = Instant::now();
    let (mut channels, sampling_rate) = ffmpeg::decode_channels(&cur_filename)?;
    info!(
        "time for ffmpeg decode: {}s, channels: {}, samples: {}",
        decode_timer.elapsed().as_secs_f32(),
        channels.len(),
        channels.first().map_or(0, |c| c.len())
    );

    let encode_timer = Instant::now();

    if let Some(end) = options.end {
        for channel in &mut channels {
            channel.truncate(end);
        }
    }

    let encoded = brstm::encoder::encode_brstm(&channels, sampling_rate, options.loop_point)
        .map_err(EncoderError::EncodingError)?;

    let mut out_buf = Vec::new();

    encoded
        .write_brstm(&mut Cursor::new(&mut out_buf))
        .map_err(|e| EncoderError::Custom(e.to_string()))?;

    info!(
        "time for brstm encode: {}s, output size: {}",
        encode_timer.elapsed().as_secs_f32(),
        out_buf.len()
    );

    Ok(Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Content-Disposition", "attachment")
        .body(out_buf.into())
        .unwrap())
}
