use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    break_to_prompt, open_session, read_until, run_sd_image, sd_copy, write_serial, BoardProfile,
};

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLES_CSS: &str = include_str!("../web/styles.css");
const APP_JS: &str = include_str!("../web/app.js");

pub(crate) struct ServeOptions {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) board: BoardProfile,
    pub(crate) board_path: PathBuf,
    pub(crate) log_dir: PathBuf,
    pub(crate) os_dir: PathBuf,
    pub(crate) image: PathBuf,
}

#[derive(Clone)]
struct AppState {
    board: BoardProfile,
    board_path: PathBuf,
    log_dir: PathBuf,
    os_dir: PathBuf,
    image: PathBuf,
}

#[derive(Serialize)]
struct StatusData {
    board: BoardProfile,
    board_path: String,
    log_dir: String,
    os_dir: String,
    image_path: String,
    image: FileStatus,
    serial: SerialStatus,
    volumes: Vec<VolumeStatus>,
    latest_log: Option<LogData>,
}

#[derive(Serialize)]
struct FileStatus {
    exists: bool,
    size: Option<u64>,
    modified: Option<String>,
    sha256: Option<String>,
}

#[derive(Serialize)]
struct SerialStatus {
    configured_port: String,
    detected_ports: Vec<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct VolumeStatus {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct LogData {
    path: String,
    text: String,
}

#[derive(Serialize)]
struct CommandResult {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[derive(Serialize)]
struct DeviceRunResult {
    log_path: String,
    log_tail: String,
    verified: bool,
}

#[derive(Deserialize)]
struct SdCopyRequest {
    volume: Option<String>,
    image: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct BreakRequest {
    seconds: Option<f32>,
}

#[derive(Deserialize)]
struct RunSdRequest {
    no_break: Option<bool>,
    run_seconds: Option<f32>,
}

pub(crate) fn serve(options: ServeOptions) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", options.host, options.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", options.host, options.port))?;
    let state = AppState {
        board: options.board,
        board_path: options.board_path,
        log_dir: options.log_dir,
        os_dir: options.os_dir,
        image: options.image,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    runtime.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
            .route("/app.css", get(styles))
            .route("/app.js", get(app_js))
            .route("/api/status", get(api_status))
            .route("/api/build-os", post(api_build_os))
            .route("/api/sd-copy", post(api_sd_copy))
            .route("/api/break", post(api_break))
            .route("/api/run-sd", post(api_run_sd))
            .route("/api/logs/latest", get(api_latest_log))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        println!("Xiaojin Lab UI: http://{addr}");
        axum::serve(listener, app)
            .await
            .context("web server failed")
    })
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLES_CSS,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
}

async fn api_status(State(state): State<AppState>) -> Response {
    match status_data(&state) {
        Ok(data) => json_ok(data),
        Err(error) => json_error(error),
    }
}

async fn api_build_os(State(state): State<AppState>) -> Response {
    match tokio::task::spawn_blocking(move || build_os_capture(&state.os_dir)).await {
        Ok(Ok(data)) => json_ok(data),
        Ok(Err(error)) => json_error(error),
        Err(error) => json_error(anyhow::anyhow!("build task failed: {error}")),
    }
}

async fn api_sd_copy(State(state): State<AppState>, Json(req): Json<SdCopyRequest>) -> Response {
    match tokio::task::spawn_blocking(move || {
        let volume = req
            .volume
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Volumes/R5COS"));
        let image = req.image.map(PathBuf::from).unwrap_or(state.image.clone());
        sd_copy(&state.board, &volume, &image, req.name.as_deref())?;
        let dest = volume.join(req.name.as_deref().unwrap_or(&state.board.image_name));
        Ok::<_, anyhow::Error>(FileStatus {
            exists: dest.exists(),
            size: fs::metadata(&dest).ok().map(|meta| meta.len()),
            modified: modified_string(&dest),
            sha256: sha256_file(&dest).ok(),
        })
    })
    .await
    {
        Ok(Ok(data)) => json_ok(data),
        Ok(Err(error)) => json_error(error),
        Err(error) => json_error(anyhow::anyhow!("sd-copy task failed: {error}")),
    }
}

async fn api_break(State(state): State<AppState>, Json(req): Json<BreakRequest>) -> Response {
    match tokio::task::spawn_blocking(move || {
        let mut session = open_session(&state.board, &state.log_dir, "web-break")?;
        let seconds = req.seconds.unwrap_or(state.board.break_seconds);
        break_to_prompt(&mut session, &state.board, Duration::from_secs_f32(seconds))?;
        let log_path = session.log_path.clone();
        Ok::<_, anyhow::Error>(DeviceRunResult {
            log_path: log_path.display().to_string(),
            log_tail: latest_log_text_at(&log_path, 180).unwrap_or_default(),
            verified: true,
        })
    })
    .await
    {
        Ok(Ok(data)) => json_ok(data),
        Ok(Err(error)) => json_error(error),
        Err(error) => json_error(anyhow::anyhow!("break task failed: {error}")),
    }
}

async fn api_run_sd(State(state): State<AppState>, Json(req): Json<RunSdRequest>) -> Response {
    match tokio::task::spawn_blocking(move || {
        let mut session = open_session(&state.board, &state.log_dir, "web-run-sd")?;
        if req.no_break.unwrap_or(false) {
            write_serial(&mut session, b"\r")?;
            read_until(&mut session, b"=>", Duration::from_secs(3))?;
        } else {
            break_to_prompt(
                &mut session,
                &state.board,
                Duration::from_secs_f32(state.board.break_seconds),
            )?;
        }
        run_sd_image(
            &mut session,
            &state.board,
            Duration::from_secs_f32(req.run_seconds.unwrap_or(8.0)),
            false,
        )?;
        let log_path = session.log_path.clone();
        Ok::<_, anyhow::Error>(DeviceRunResult {
            log_path: log_path.display().to_string(),
            log_tail: latest_log_text_at(&log_path, 220).unwrap_or_default(),
            verified: true,
        })
    })
    .await
    {
        Ok(Ok(data)) => json_ok(data),
        Ok(Err(error)) => json_error(error),
        Err(error) => json_error(anyhow::anyhow!("run-sd task failed: {error}")),
    }
}

async fn api_latest_log(State(state): State<AppState>) -> Response {
    match latest_log_data(&state.log_dir, 180) {
        Ok(data) => json_ok(data),
        Err(error) => json_error(error),
    }
}

fn status_data(state: &AppState) -> Result<StatusData> {
    Ok(StatusData {
        board: state.board.clone(),
        board_path: path_text(&state.board_path),
        log_dir: path_text(&state.log_dir),
        os_dir: path_text(&state.os_dir),
        image_path: path_text(&state.image),
        image: file_status(&state.image),
        serial: serial_status(&state.board.port),
        volumes: list_volumes(),
        latest_log: latest_log_data(&state.log_dir, 120).ok().flatten(),
    })
}

fn build_os_capture(os_dir: &Path) -> Result<CommandResult> {
    let output = Command::new("./build.sh")
        .current_dir(os_dir)
        .output()
        .with_context(|| format!("failed to run {}/build.sh", os_dir.display()))?;
    Ok(CommandResult {
        ok: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn file_status(path: &Path) -> FileStatus {
    let metadata = fs::metadata(path).ok();
    FileStatus {
        exists: metadata.is_some(),
        size: metadata.as_ref().map(|meta| meta.len()),
        modified: modified_string(path),
        sha256: sha256_file(path).ok(),
    }
}

fn serial_status(configured_port: &str) -> SerialStatus {
    match serialport::available_ports() {
        Ok(ports) => SerialStatus {
            configured_port: configured_port.to_owned(),
            detected_ports: ports.into_iter().map(|port| port.port_name).collect(),
            error: None,
        },
        Err(error) => SerialStatus {
            configured_port: configured_port.to_owned(),
            detected_ports: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

fn list_volumes() -> Vec<VolumeStatus> {
    fs::read_dir("/Volumes")
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            Some(VolumeStatus {
                name,
                path: path.display().to_string(),
            })
        })
        .collect()
}

fn latest_log_data(log_dir: &Path, lines: usize) -> Result<Option<LogData>> {
    let Some(path) = latest_log_path(log_dir)? else {
        return Ok(None);
    };
    let text = latest_log_text_at(&path, lines)?;
    Ok(Some(LogData {
        path: path.display().to_string(),
        text,
    }))
}

fn latest_log_path(log_dir: &Path) -> Result<Option<PathBuf>> {
    if !log_dir.exists() {
        return Ok(None);
    }
    let mut entries = fs::read_dir(log_dir)
        .with_context(|| format!("failed to read log dir {}", log_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
    Ok(entries.pop().map(|entry| entry.path()))
}

fn latest_log_text_at(path: &Path, lines: usize) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let all_lines = text.lines().collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines);
    Ok(all_lines[start..].join("\n"))
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn modified_string(path: &Path) -> Option<String> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(format_time)
}

fn format_time(time: SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();
    datetime.to_rfc3339()
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

fn json_ok<T: Serialize>(data: T) -> Response {
    Json(json!({ "ok": true, "data": data })).into_response()
}

fn json_error(error: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "ok": false, "error": error.to_string() })),
    )
        .into_response()
}
