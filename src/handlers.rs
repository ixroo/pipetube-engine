use crate::models::{
    ApiResponse, InfoRequest, ProxyQuery, ResolveRequest, ResolveResponse, VideoInfo,
};
use crate::rate_limiter::RateLimiter;
use crate::ytdl::{YtdlError, YtdlManager};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::Client;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};
use url::Url;

#[derive(Clone)]
pub struct AppState {
    pub ytdl: Arc<YtdlManager>,
    pub rate_limiter: Arc<RateLimiter>,
    pub http_client: Client,
}

/// Handler for POST /api/info
pub async fn get_video_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<InfoRequest>,
) -> Result<Json<ApiResponse<VideoInfo>>, (StatusCode, Json<ApiResponse<()>>)> {
    let client_ip = RateLimiter::extract_ip(&headers);
    if let Err((status, msg)) = state.rate_limiter.check(client_ip).await {
        return Err((status, Json(ApiResponse::err("RATE_LIMITED", msg))));
    }

    match state.ytdl.fetch_info(&payload.url).await {
        Ok(info) => Ok(Json(ApiResponse::ok(info))),
        Err(err) => {
            let (status, code, message) = map_ytdl_error(err);
            Err((status, Json(ApiResponse::err(code, message))))
        }
    }
}

/// Handler for POST /api/resolve
pub async fn resolve_format(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ResolveRequest>,
) -> Result<Json<ApiResponse<ResolveResponse>>, (StatusCode, Json<ApiResponse<()>>)> {
    let client_ip = RateLimiter::extract_ip(&headers);
    if let Err((status, msg)) = state.rate_limiter.check(client_ip).await {
        return Err((status, Json(ApiResponse::err("RATE_LIMITED", msg))));
    }

    let is_audio = payload.format_id.to_lowercase().contains("audio")
        || ["139", "140", "249", "250", "251", "ba"].iter().any(|k| payload.format_id.contains(k));
    let ext = if is_audio { "m4a" } else { "mp4" };

    // Use sanitized video title for customer download filename
    let raw_title = payload.title.as_deref().unwrap_or("video");
    let safe_title: String = raw_title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let safe_title = safe_title.trim_matches(&[' ', '.'] as &[_]);
    let final_title = if safe_title.is_empty() { "video" } else { safe_title };
    let filename = format!("{}.{}", final_title, ext);

    match state.ytdl.resolve_stream(&payload.url, &payload.format_id).await {
        Ok(direct_url) => {
            let encoded_url = utf8_percent_encode(&payload.url, NON_ALPHANUMERIC).to_string();
            let encoded_name = utf8_percent_encode(&filename, NON_ALPHANUMERIC).to_string();
            let audio_param = if is_audio { "&audio=1" } else { "" };
            let proxy_url = format!("/api/proxy?url={}&format_id={}{}&filename={}", encoded_url, payload.format_id, audio_param, encoded_name);

            Ok(Json(ApiResponse::ok(ResolveResponse {
                direct_url,
                proxy_url,
                format_id: payload.format_id,
                filename,
            })))
        }
        Err(err) => {
            let (status, code, message) = map_ytdl_error(err);
            Err((status, Json(ApiResponse::err(code, message))))
        }
    }
}

/// Handler for GET /api/proxy (Zero-disk, pure async byte pipe)
pub async fn stream_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProxyQuery>,
) -> Response {
    let client_ip = RateLimiter::extract_ip(&headers);
    if let Err((status, msg)) = state.rate_limiter.check(client_ip).await {
        return (status, Json(ApiResponse::<()>::err("RATE_LIMITED", msg))).into_response();
    }

    // High-performance media caching and chunked downloader pipeline
    if let Some(ref raw_fid) = query.format_id {
        // Strict URL and format_id validation: prevent parameter injection & invalid targets
        let canonical_url = match state.ytdl.validate_url(&query.url) {
            Ok(u) => u,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ApiResponse::<()>::err("INVALID_URL", "Invalid YouTube video URL.")),
                )
                    .into_response();
            }
        };

        let fid = match state.ytdl.validate_format_id(raw_fid) {
            Ok(f) if !f.starts_with('-') => f,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ApiResponse::<()>::err("INVALID_FORMAT_ID", "Format identifier is invalid.")),
                )
                    .into_response();
            }
        };

        let is_audio = query.audio.as_deref() == Some("1");
        let ext = if is_audio { "m4a" } else { "mp4" };
        let default_name = if is_audio { "audio.m4a" } else { "video.mp4" };
        let safe_filename = query
            .filename
            .clone()
            .unwrap_or_else(|| default_name.to_string());
        let disposition = format!("attachment; filename=\"{}\"", safe_filename);

        // Extract video ID from canonical URL (e.g. https://www.youtube.com/watch?v=XYZ -> XYZ)
        let video_id = canonical_url
            .split("v=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap_or("media");

        let cache_dir = std::path::Path::new("/tmp/pipetube_cache");
        let _ = tokio::fs::create_dir_all(cache_dir).await;

        let cache_file = cache_dir.join(format!("{}_{}.{}", video_id, fid, ext));
        let temp_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let temp_pattern = cache_dir.join(format!("{}_{}_{}.tmp.%(ext)s", video_id, fid, temp_suffix));
        let temp_prefix = format!("{}_{}_{}.tmp.", video_id, fid, temp_suffix);

        // 1. Check Cache Hit: If already processed and non-empty, serve immediately (0ms latency)
        let mut is_cached = false;
        if let Ok(meta) = tokio::fs::metadata(&cache_file).await {
            if meta.len() > 1024 {
                info!("Cache HIT: Serving pre-processed stream from {:?}", cache_file);
                is_cached = true;
            }
        }

        // 2. Cache Miss: Execute fast chunked yt-dlp download & multiplexing
        if !is_cached {
            info!("Cache MISS: Downloading media via high-speed chunked engine: {} [Format: {}]", canonical_url, fid);

            let mut cmd = Command::new("yt-dlp");
            if is_audio {
                cmd.args([
                    "--no-warnings",
                    "--no-playlist",
                    "-f",
                    &format!("{}/ba[ext=m4a]/ba", fid),
                    "--force-ipv4",
                    "-o",
                    temp_pattern.to_str().unwrap(),
                    &canonical_url,
                ]);
            } else {
                cmd.args([
                    "--no-warnings",
                    "--no-playlist",
                    "-f",
                    &format!("{}+ba/{}+140/best", fid, fid),
                    "--merge-output-format",
                    "mp4",
                    "--force-ipv4",
                    "-o",
                    temp_pattern.to_str().unwrap(),
                    &canonical_url,
                ]);
            }

            if std::path::Path::new("/opt/ytdl-raw/cookies.txt").exists() {
                cmd.arg("--cookies").arg("/opt/ytdl-raw/cookies.txt");
            }
            cmd.arg("--js-runtimes").arg("node");
            cmd.kill_on_drop(true);

            let download_result = tokio::time::timeout(Duration::from_secs(90), cmd.output()).await;

            match download_result {
                Ok(Ok(out)) => {
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        error!("yt-dlp chunked download failed: {}", stderr);
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(ApiResponse::<()>::err("DOWNLOAD_FAILED", format!("Download failed: {}", stderr.lines().next().unwrap_or("extraction error")))),
                        ).into_response();
                    }

                    // Locate the downloaded output file matching temp_prefix
                    let mut found_file: Option<std::path::PathBuf> = None;
                    if let Ok(mut entries) = tokio::fs::read_dir(cache_dir).await {
                        while let Ok(Some(entry)) = entries.next_entry().await {
                            if let Some(name) = entry.file_name().to_str() {
                                if name.starts_with(&temp_prefix) {
                                    found_file = Some(entry.path());
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(src) = found_file {
                        if let Err(e) = tokio::fs::rename(&src, &cache_file).await {
                            warn!("Failed to rename temp file {:?} to {:?}: {}", src, cache_file, e);
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("Failed to spawn yt-dlp child process: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiResponse::<()>::err("EXEC_FAILED", "Failed to launch media processor.")),
                    ).into_response();
                }
                Err(_) => {
                    error!("yt-dlp execution timed out after 90 seconds for URL: {}", canonical_url);
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        Json(ApiResponse::<()>::err("TIMEOUT", "YouTube media download timed out.")),
                    ).into_response();
                }
            }
        }

        // 3. Open cached file and stream to client with exact Content-Length
        match tokio::fs::File::open(&cache_file).await {
            Ok(file) => {
                let file_len = match file.metadata().await {
                    Ok(m) => m.len(),
                    Err(_) => 0,
                };

                let stream = ReaderStream::new(file);
                let content_type = if is_audio { "audio/mp4" } else { "video/mp4" };

                let mut builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_DISPOSITION, disposition)
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::CACHE_CONTROL, "public, max-age=3600");

                if file_len > 0 {
                    builder = builder.header(header::CONTENT_LENGTH, file_len.to_string());
                }

                if let Some(ref token) = query.token {
                    builder = builder.header(
                        header::SET_COOKIE,
                        format!("dl_token_{}=1; Path=/; SameSite=Lax", token),
                    );
                }

                return builder
                    .body(Body::from_stream(stream))
                    .unwrap_or_else(|_| {
                        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build stream response").into_response()
                    });
            }
            Err(e) => {
                error!("Failed to read processed media file {:?}: {}", cache_file, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse::<()>::err("STREAM_READ_ERROR", "Could not read processed media file.")),
                ).into_response();
            }
        }
    }

    // SSRF Defense: Verify destination host is an authentic Google Video or YouTube CDN
    let target_url = match Url::parse(&query.url) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<()>::err("INVALID_PROXY_URL", "Supplied URL is malformed.")),
            )
                .into_response();
        }
    };

    let host = target_url.host_str().unwrap_or("");
    let is_authorized_cdn = host.ends_with(".googlevideo.com")
        || host == "googlevideo.com"
        || host.ends_with(".youtube.com")
        || host == "youtube.com"
        || host.ends_with("youtu.be")
        || host == "youtu.be";

    if !is_authorized_cdn {
        warn!("SSRF attempt blocked for target host: {}", host);
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::<()>::err(
                "SSRF_DENIED",
                "Destination is not an authorized video streaming CDN.",
            )),
        )
            .into_response();
    }

    info!("Streaming async proxy connection for CDN target: {}", host);

    // Invoke upstream request with streaming client
    let mut req_builder = state
        .http_client
        .get(query.url.as_str())
        .header(
            header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        );

    // Forward Range header from browser to prevent Google CDN throttling & enable parallel chunks
    if let Some(range) = headers.get(header::RANGE) {
        req_builder = req_builder.header(header::RANGE, range);
    }

    let upstream_res = match req_builder.send().await {
        Ok(res) => res,
        Err(e) => {
            error!("Upstream CDN connection error: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ApiResponse::<()>::err("UPSTREAM_GATEWAY_ERROR", "Failed to connect to media CDN.")),
            )
                .into_response();
        }
    };

    if !upstream_res.status().is_success() {
        let code = upstream_res.status();
        warn!("Upstream CDN returned non-200 code: {}", code);
        return (
            StatusCode::BAD_GATEWAY,
            Json(ApiResponse::<()>::err(
                "CDN_EXPIRED",
                "Upstream CDN link expired or access denied. Please re-resolve the format.",
            )),
        )
            .into_response();
    }

    // Extract headers for forced file download
    let content_type = upstream_res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let content_length = upstream_res.headers().get(header::CONTENT_LENGTH).cloned();
    let content_range = upstream_res.headers().get(header::CONTENT_RANGE).cloned();

    // Sanitize and format filename
    let raw_name = query.filename.unwrap_or_else(|| "download.mp4".to_string());
    let safe_filename: String = raw_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();

    let disposition = format!("attachment; filename=\"{}\"", safe_filename);

    let status = if upstream_res.status() == StatusCode::PARTIAL_CONTENT {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut response_builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store, no-cache, must-revalidate");

    if let Some(cl) = content_length {
        response_builder = response_builder.header(header::CONTENT_LENGTH, cl);
    }
    if let Some(cr) = content_range {
        response_builder = response_builder.header(header::CONTENT_RANGE, cr);
    }

    // Pure async streaming pipe: zero memory buffer, zero disk storage
    let stream = upstream_res.bytes_stream();
    let body = Body::from_stream(stream);

    response_builder.body(body).unwrap_or_else(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse::<()>::err("STREAM_BUILD_ERROR", e.to_string())),
        )
            .into_response()
    })
}

fn map_ytdl_error(err: YtdlError) -> (StatusCode, &'static str, String) {
    match err {
        YtdlError::InvalidUrl(msg) => (StatusCode::BAD_REQUEST, "INVALID_URL", msg),
        YtdlError::ServerBusy => (
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVER_BUSY",
            "Server is experiencing high traffic. Please retry in 5 seconds.".to_string(),
        ),
        YtdlError::ExecutionTimeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "TIMEOUT",
            "Extraction timed out. YouTube took too long to respond.".to_string(),
        ),
        YtdlError::ProcessFailed { code, message } => {
            (StatusCode::UNPROCESSABLE_ENTITY, "EXTRACTION_FAILED", format!("{}: {}", code, message))
        }
        YtdlError::JsonParseError(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "METADATA_PARSE_ERROR",
            format!("Metadata parsing error: {}", msg),
        ),
    }
}
