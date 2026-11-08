use axum::http::{HeaderMap, StatusCode};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

#[derive(Debug, Clone)]
struct ClientBucket {
    count: u32,
    window_start: Instant,
}

#[derive(Debug, Clone)]
pub struct RateLimiter {
    state: Arc<Mutex<HashMap<IpAddr, ClientBucket>>>,
    max_requests: u32,
    window_duration: Duration,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window_duration: Duration) -> Self {
        let state = Arc::new(Mutex::new(HashMap::new()));
        let limiter = Self {
            state: state.clone(),
            max_requests,
            window_duration,
        };

        // Background garbage collection task to prevent memory leaks on 1GB VPS
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                let mut map = state.lock().await;
                let now = Instant::now();
                map.retain(|_, bucket| now.duration_since(bucket.window_start) < Duration::from_secs(120));
            }
        });

        limiter
    }

    /// Extract client IP prioritizing Cloudflare CF-Connecting-IP, then X-Forwarded-For
    pub fn extract_ip(headers: &HeaderMap) -> IpAddr {
        if let Some(cf_ip) = headers.get("cf-connecting-ip") {
            if let Ok(ip_str) = cf_ip.to_str() {
                if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }

        if let Some(x_forwarded) = headers.get("x-forwarded-for") {
            if let Ok(ips_str) = x_forwarded.to_str() {
                if let Some(first_ip) = ips_str.split(',').next() {
                    if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                        return ip;
                    }
                }
            }
        }

        if let Some(real_ip) = headers.get("x-real-ip") {
            if let Ok(ip_str) = real_ip.to_str() {
                if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }

        // Local fallback when running directly behind standard loopback proxy
        "127.0.0.1".parse().unwrap()
    }

    /// Check and increment the request count for the given IP address
    pub async fn check(&self, ip: IpAddr) -> Result<(), (StatusCode, String)> {
        let mut map = self.state.lock().await;
        let now = Instant::now();

        let bucket = map.entry(ip).or_insert_with(|| ClientBucket {
            count: 0,
            window_start: now,
        });

        if now.duration_since(bucket.window_start) >= self.window_duration {
            bucket.count = 0;
            bucket.window_start = now;
        }

        if bucket.count >= self.max_requests {
            warn!("Rate limit exceeded for client IP: {}", ip);
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::to_string(&crate::models::ApiResponse::<()>::err(
                    "RATE_LIMITED",
                    format!(
                        "Request threshold reached (max {} req/min). Please wait before retrying.",
                        self.max_requests
                    ),
                ))
                .unwrap_or_default(),
            ));
        }

        bucket.count += 1;
        Ok(())
    }
}
