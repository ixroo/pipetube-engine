use crate::models::{FormatOption, VideoInfo, YtdlRawOutput};
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{RwLock, Semaphore};
use tracing::{error, info, warn};

#[derive(Clone)]
struct CachedVideoInfo {
    info: VideoInfo,
    cached_at: Instant,
}

#[derive(Debug)]
pub enum YtdlError {
    InvalidUrl(String),
    ServerBusy,
    ExecutionTimeout,
    ProcessFailed { code: String, message: String },
    JsonParseError(String),
}

impl std::error::Error for YtdlError {}

// Custom manual error mapping for zero excess macro dependencies
impl std::fmt::Display for YtdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(msg) => write!(f, "Invalid YouTube URL: {}", msg),
            Self::ServerBusy => write!(f, "Server concurrency queue is full. Try again shortly."),
            Self::ExecutionTimeout => write!(f, "Stream extraction timed out (15s limit)."),
            Self::ProcessFailed { message, .. } => write!(f, "Extraction failed: {}", message),
            Self::JsonParseError(msg) => write!(f, "Failed to parse metadata: {}", msg),
        }
    }
}

pub struct YtdlManager {
    semaphore: Arc<Semaphore>,
    url_regex: Regex,
    format_id_regex: Regex,
    stream_cache: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    info_cache: Arc<RwLock<HashMap<String, CachedVideoInfo>>>,
}

impl YtdlManager {
    pub fn new(max_concurrency: usize) -> Self {
        let url_regex = Regex::new(
            r"^(https?://)?(www\.|m\.)?(youtube\.com/(watch\?.*v=|shorts/|embed/|v/|live/)|youtu\.be/)([a-zA-Z0-9_-]{11})"
        ).expect("Valid regex");

        let format_id_regex = Regex::new(r"^[a-zA-Z0-9_\-\.\+\[\]\/\=\^\:\,]+$").expect("Valid regex");

        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrency)),
            url_regex,
            format_id_regex,
            stream_cache: Arc::new(RwLock::new(HashMap::new())),
            info_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Strict validation of YouTube URL and extraction of clean canonical link
    pub fn validate_url(&self, raw_url: &str) -> Result<String, YtdlError> {
        let trimmed = raw_url.trim();
        if trimmed.is_empty() {
            return Err(YtdlError::InvalidUrl("URL cannot be empty.".to_string()));
        }

        if let Some(caps) = self.url_regex.captures(trimmed) {
            if let Some(video_id) = caps.get(5) {
                // Return clean, canonical watch URL to prevent argument injection
                return Ok(format!("https://www.youtube.com/watch?v={}", video_id.as_str()));
            }
        }

        Err(YtdlError::InvalidUrl(
            "Please provide a valid YouTube video or Shorts link (e.g., https://youtu.be/ID or youtube.com/watch?v=ID).".to_string()
        ))
    }

    /// Strict validation of requested format_id
    pub fn validate_format_id(&self, format_id: &str) -> Result<String, YtdlError> {
        let trimmed = format_id.trim();
        if self.format_id_regex.is_match(trimmed) {
            Ok(trimmed.to_string())
        } else {
            Err(YtdlError::ProcessFailed {
                code: "INVALID_FORMAT_ID".to_string(),
                message: "Format identifier contains invalid characters.".to_string(),
            })
        }
    }

    /// Execute yt-dlp metadata extraction with concurrency & timeout protection
    pub async fn fetch_info(&self, raw_url: &str) -> Result<VideoInfo, YtdlError> {
        let canonical_url = self.validate_url(raw_url)?;

        // 1. Fast in-memory metadata cache lookup (0.001s instant response, 15-min TTL)
        {
            let cache = self.info_cache.read().await;
            if let Some(cached) = cache.get(&canonical_url) {
                if cached.cached_at.elapsed() < Duration::from_secs(900) {
                    info!("Metadata cache HIT (0ms) for canonical URL: {}", canonical_url);
                    return Ok(cached.info.clone());
                }
            }
        }

        // Acquire concurrency permit with 5-second queue timeout
        let _permit = tokio::time::timeout(Duration::from_secs(5), self.semaphore.acquire())
            .await
            .map_err(|_| YtdlError::ServerBusy)?
            .map_err(|_| YtdlError::ServerBusy)?;

        info!("Executing yt-dlp metadata dump for: {}", canonical_url);

        let mut child = Command::new("yt-dlp");
        child
            .arg("--dump-json")
            .arg("--no-warnings")
            .arg("--no-playlist")
            .arg("--skip-download")
            .arg("--force-ipv4")
            .arg(&canonical_url);

        if std::path::Path::new("/opt/ytdl-raw/cookies.txt").exists() {
            child.arg("--cookies").arg("/opt/ytdl-raw/cookies.txt");
        }
        child.arg("--js-runtimes").arg("node");

        child.kill_on_drop(true);

        // 15-second hard timeout
        let output = tokio::time::timeout(Duration::from_secs(15), child.output())
            .await
            .map_err(|_| {
                warn!("yt-dlp timed out for: {}", canonical_url);
                YtdlError::ExecutionTimeout
            })?
            .map_err(|e| YtdlError::ProcessFailed {
                code: "SUBPROCESS_EXEC_ERROR".to_string(),
                message: format!("Failed to invoke yt-dlp: {}", e),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("yt-dlp stderr: {}", stderr);
            return Err(self.map_stderr_to_error(&stderr));
        }

        let raw_json: YtdlRawOutput = serde_json::from_slice(&output.stdout)
            .map_err(|e| YtdlError::JsonParseError(e.to_string()))?;

        // Cache all extracted stream URLs directly in memory for 0ms resolution
        {
            let mut map = HashMap::new();
            let mut best_audio: Option<String> = None;
            let mut best_audio_abr: f64 = -1.0;

            for f in &raw_json.formats {
                if let (Some(ref id), Some(ref u)) = (&f.format_id, &f.url) {
                    map.insert(id.clone(), u.clone());

                    let vcodec = f.vcodec.as_deref().unwrap_or("none").to_lowercase();
                    let acodec = f.acodec.as_deref().unwrap_or("none").to_lowercase();
                    let ext = f.ext.as_deref().unwrap_or("").to_lowercase();
                    let abr = f.abr.unwrap_or(0.0);

                    if vcodec == "none" && acodec != "none" {
                        if ext == "m4a" || acodec.contains("mp4a") {
                            if abr > best_audio_abr {
                                best_audio_abr = abr;
                                best_audio = Some(u.clone());
                            }
                        } else if best_audio.is_none() {
                            best_audio = Some(u.clone());
                        }
                    }
                }
            }

            if let Some(ba) = best_audio {
                map.insert("best_audio".to_string(), ba);
            }

            let mut cache = self.stream_cache.write().await;
            cache.insert(canonical_url.clone(), map);
        }

        let info = self.transform_raw_output(raw_json)?;

        // Cache parsed VideoInfo for 15 minutes (protects against viral load & repeat queries)
        {
            let mut cache = self.info_cache.write().await;
            cache.insert(canonical_url, CachedVideoInfo {
                info: info.clone(),
                cached_at: Instant::now(),
            });
        }

        Ok(info)
    }

    /// Resolve exact direct CDN stream URL for a given format
    #[allow(dead_code)]
    pub async fn resolve_stream(&self, raw_url: &str, raw_format_id: &str) -> Result<String, YtdlError> {
        let canonical_url = self.validate_url(raw_url)?;
        let format_id = self.validate_format_id(raw_format_id)?;

        // Check in-memory stream cache first for instant 0ms response
        {
            let cache = self.stream_cache.read().await;
            if let Some(formats) = cache.get(&canonical_url) {
                if let Some(cached_url) = formats.get(&format_id) {
                    info!("Stream CDN cache HIT for URL: {}, Format: {}", canonical_url, format_id);
                    return Ok(cached_url.clone());
                }
            }
        }

        let _permit = tokio::time::timeout(Duration::from_secs(5), self.semaphore.acquire())
            .await
            .map_err(|_| YtdlError::ServerBusy)?
            .map_err(|_| YtdlError::ServerBusy)?;

        info!("Resolving stream CDN for URL: {}, Format: {}", canonical_url, format_id);

        let effective_format = if format_id == "best_audio" {
            "bestaudio[ext=m4a]/ba[acodec^=mp4a]/ba"
        } else {
            &format_id
        };

        let mut child = Command::new("yt-dlp");
        child
            .arg("-g")
            .arg("-f")
            .arg(effective_format)
            .arg("--no-playlist")
            .arg("--force-ipv4")
            .arg(&canonical_url);

        if std::path::Path::new("/opt/ytdl-raw/cookies.txt").exists() {
            child.arg("--cookies").arg("/opt/ytdl-raw/cookies.txt");
        }
        child.arg("--js-runtimes").arg("node");

        child.kill_on_drop(true);

        let output = tokio::time::timeout(Duration::from_secs(15), child.output())
            .await
            .map_err(|_| YtdlError::ExecutionTimeout)?
            .map_err(|e| YtdlError::ProcessFailed {
                code: "SUBPROCESS_EXEC_ERROR".to_string(),
                message: format!("Failed to invoke yt-dlp: {}", e),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("yt-dlp resolve stderr: {}", stderr);
            return Err(self.map_stderr_to_error(&stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let direct_url = stdout.lines().next().unwrap_or("").trim().to_string();

        if direct_url.is_empty() || !direct_url.starts_with("http") {
            return Err(YtdlError::ProcessFailed {
                code: "EMPTY_CDN_URL".to_string(),
                message: "Resolved stream returned an empty or invalid CDN URL.".to_string(),
            });
        }

        Ok(direct_url)
    }

    fn transform_raw_output(&self, raw: YtdlRawOutput) -> Result<VideoInfo, YtdlError> {
        let duration_secs = raw.duration.unwrap_or(0.0).round() as u64;
        let duration_formatted = format_duration(duration_secs);

        fn codec_rank(vcodec: &str) -> u8 {
            let vc = vcodec.to_lowercase();
            if vc.contains("avc") || vc.contains("h264") {
                3 // H.264: Universal hardware decode & full audio player compatibility
            } else if vc.contains("vp09") || vc.contains("vp9") {
                2 // VP9: High definition / 4K UHD
            } else if vc.contains("av01") {
                1 // AV1
            } else {
                0
            }
        }

        use std::collections::HashMap;
        let mut best_videos: HashMap<u32, (FormatOption, u8, u64, bool)> = HashMap::new();
        let mut audio_options: Vec<FormatOption> = Vec::new();

        for f in raw.formats {
            let protocol = f.protocol.unwrap_or_default();
            if protocol.contains("mhtml") {
                continue;
            }

            let format_id = match f.format_id {
                Some(id) if !id.is_empty() => id,
                _ => continue,
            };

            let ext = f.ext.unwrap_or_else(|| "mp4".to_string());
            let vcodec = f.vcodec.unwrap_or_else(|| "none".to_string());
            let acodec = f.acodec.unwrap_or_else(|| "none".to_string());

            let is_audio_only = vcodec == "none" && acodec != "none";
            let is_video_only = vcodec != "none" && acodec == "none";
            let is_muxed = vcodec != "none" && acodec != "none";

            if !is_audio_only && !is_video_only && !is_muxed {
                continue;
            }

            let height = f.height.unwrap_or(0);
            let filesize = f.filesize.or(f.filesize_approx);
            let filesize_formatted = format_filesize(filesize);
            let size_bytes = filesize.unwrap_or(0);
            let is_hls = protocol.contains("m3u8");

            if (is_video_only || is_muxed) && height >= 144 {
                let rank = codec_rank(&vcodec);
                let replace = match best_videos.get(&height) {
                    None => true,
                    Some((_, _, _, prev_hls)) if *prev_hls && !is_hls => true,
                    Some((_, _, _, prev_hls)) if !*prev_hls && is_hls => false,
                    Some((_, prev_rank, _, _)) if rank > *prev_rank => true,
                    Some((_, prev_rank, prev_size, _)) if rank == *prev_rank && size_bytes > *prev_size => true,
                    _ => false,
                };

                if replace {
                    let codec_name = if rank == 3 {
                        "H.264"
                    } else if rank == 2 {
                        "VP9"
                    } else if rank == 1 {
                        "AV1"
                    } else {
                        "MP4"
                    };

                    let format_note = if is_video_only {
                        format!("{}p [{} • Full Audio Muxed]", height, codec_name)
                    } else {
                        format!("{}p [Direct Muxed]", height)
                    };

                    let opt = FormatOption {
                        format_id,
                        ext: if rank == 3 || is_muxed { "mp4".to_string() } else { ext },
                        resolution: format!("{}p", height),
                        fps: f.fps.map(|v| v.round() as u32),
                        filesize,
                        filesize_formatted,
                        format_note,
                        is_audio_only: false,
                        is_video_only,
                        is_muxed: true, // Served fully muxed with AAC audio
                        quality_label: format!("{}p ({} + AUDIO)", height, codec_name),
                    };
                    best_videos.insert(height, (opt, rank, size_bytes, is_hls));
                }
            } else if is_audio_only {
                let quality_label = if let Some(ref note) = f.format_note {
                    format!("AUDIO [{}]", note.to_uppercase())
                } else {
                    format!("AUDIO [{}]", ext.to_uppercase())
                };

                audio_options.push(FormatOption {
                    format_id,
                    ext,
                    resolution: "Audio Only".to_string(),
                    fps: None,
                    filesize,
                    filesize_formatted,
                    format_note: f.format_note.unwrap_or_default(),
                    is_audio_only: true,
                    is_video_only: false,
                    is_muxed: false,
                    quality_label,
                });
            }
        }

        // Sort videos highest resolution first
        let mut sorted_heights: Vec<u32> = best_videos.keys().cloned().collect();
        sorted_heights.sort_by(|a, b| b.cmp(a));

        let mut options: Vec<FormatOption> = Vec::new();
        for h in sorted_heights {
            if let Some((opt, _, _, _)) = best_videos.remove(&h) {
                options.push(opt);
            }
        }

        // Add top audio options (M4A and WebM)
        let m4a_audio = audio_options.iter().find(|a| a.ext == "m4a").cloned();
        let other_audio = audio_options.iter().find(|a| a.ext != "m4a").cloned();
        if let Some(a) = m4a_audio {
            options.push(a);
        }
        if let Some(a) = other_audio {
            options.push(a);
        }

        Ok(VideoInfo {
            id: raw.id.unwrap_or_else(|| "unknown".to_string()),
            title: raw.title.unwrap_or_else(|| "Untitled Media".to_string()),
            thumbnail: raw.thumbnail.unwrap_or_default(),
            duration: duration_secs,
            duration_formatted,
            uploader: raw.uploader.unwrap_or_else(|| "Unknown Creator".to_string()),
            channel_url: raw.channel_url,
            view_count: raw.view_count,
            formats: options,
        })
    }

    fn map_stderr_to_error(&self, stderr: &str) -> YtdlError {
        let err_lower = stderr.to_lowercase();
        if err_lower.contains("private video") {
            YtdlError::ProcessFailed {
                code: "PRIVATE_VIDEO".to_string(),
                message: "This video is private and cannot be extracted.".to_string(),
            }
        } else if err_lower.contains("sign in to confirm your age") || err_lower.contains("age-restricted") {
            YtdlError::ProcessFailed {
                code: "AGE_RESTRICTED".to_string(),
                message: "This video is age-restricted and requires account verification.".to_string(),
            }
        } else if err_lower.contains("video unavailable") || err_lower.contains("this video has been removed") {
            YtdlError::ProcessFailed {
                code: "VIDEO_UNAVAILABLE".to_string(),
                message: "Video is unavailable or has been removed from YouTube.".to_string(),
            }
        } else if err_lower.contains("not available in your country") || err_lower.contains("geo-restricted") {
            YtdlError::ProcessFailed {
                code: "GEO_RESTRICTED".to_string(),
                message: "This video is geo-restricted in the server's region.".to_string(),
            }
        } else if err_lower.contains("too many requests") || err_lower.contains("bot") || err_lower.contains("captcha") {
            YtdlError::ProcessFailed {
                code: "UPSTREAM_RATE_LIMITED".to_string(),
                message: "YouTube upstream rate limit or bot check encountered. Retry in a few moments.".to_string(),
            }
        } else {
            // First 150 chars of sanitized stderr
            let summary: String = stderr.lines().next().unwrap_or("Unknown upstream error").chars().take(150).collect();
            YtdlError::ProcessFailed {
                code: "YTDL_ERROR".to_string(),
                message: format!("Extraction error: {}", summary),
            }
        }
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, secs)
    } else {
        format!("{:02}:{:02}", minutes, secs)
    }
}

fn format_filesize(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) if b >= 1_073_741_824 => format!("{:.1} GB", b as f64 / 1_073_741_824.0),
        Some(b) if b >= 1_048_576 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        Some(b) if b >= 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        Some(b) => format!("{} B", b),
        None => "Dynamic".to_string(),
    }
}
