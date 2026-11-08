use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct InfoRequest {
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    pub url: String,
    pub format_id: String,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProxyQuery {
    pub url: String,
    pub filename: Option<String>,
    pub format_id: Option<String>,
    pub audio: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiErrorPayload>,
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(ApiErrorPayload {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApiErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct VideoInfo {
    pub id: String,
    pub title: String,
    pub thumbnail: String,
    pub duration: u64,
    pub duration_formatted: String,
    pub uploader: String,
    pub channel_url: Option<String>,
    pub view_count: Option<u64>,
    pub formats: Vec<FormatOption>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FormatOption {
    pub format_id: String,
    pub ext: String,
    pub resolution: String,
    pub fps: Option<u32>,
    pub filesize: Option<u64>,
    pub filesize_formatted: String,
    pub format_note: String,
    pub is_audio_only: bool,
    pub is_video_only: bool,
    pub is_muxed: bool,
    pub quality_label: String,
}

#[derive(Debug, Serialize)]
pub struct ResolveResponse {
    pub direct_url: String,
    pub proxy_url: String,
    pub format_id: String,
    pub filename: String,
}

/// Raw JSON output structure from `yt-dlp --dump-json`
#[derive(Debug, Deserialize)]
pub struct YtdlRawOutput {
    pub id: Option<String>,
    pub title: Option<String>,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub uploader: Option<String>,
    pub channel_url: Option<String>,
    pub view_count: Option<u64>,
    #[serde(default)]
    pub formats: Vec<YtdlRawFormat>,
}

#[derive(Debug, Deserialize)]
pub struct YtdlRawFormat {
    pub format_id: Option<String>,
    pub ext: Option<String>,
    pub resolution: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub filesize: Option<u64>,
    pub filesize_approx: Option<u64>,
    pub format_note: Option<String>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub protocol: Option<String>,
    pub url: Option<String>,
    pub abr: Option<f64>,
}
