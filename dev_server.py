#!/usr/bin/env python3
"""
YTDL-RAW // LOCAL ZERO-DEPENDENCY DEV SERVER
Used for instantaneous local testing on Windows without needing Rust/MSVC compiler.
Pure Python standard library (Zero extra pip packages required).
"""

import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.parse
import urllib.request

PORT = int(os.environ.get("PORT", 3000))
PUBLIC_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "public")
YT_REGEX = re.compile(
    r"^(https?://)?(www\.|m\.)?(youtube\.com/(watch\?.*v=|shorts/|embed/|v/|live/)|youtu\.be/)([a-zA-Z0-9_-]{11})"
)

# Ultra-fast in-memory stream cache: video_id -> { "formats": { fid: url }, "best_audio": url }
STREAM_CACHE = {}
# 15-Minute in-memory metadata cache: video_id -> (timestamp, response_payload)
INFO_CACHE = {}


def find_ytdl_bin():
    found = shutil.which("yt-dlp") or shutil.which("yt-dlp.exe")
    if found:
        return found
    local_app = os.environ.get("LOCALAPPDATA", "")
    if local_app:
        for root, _, files in os.walk(os.path.join(local_app, "Microsoft", "WinGet", "Packages")):
            if "yt-dlp.exe" in files:
                found_path = os.path.join(root, "yt-dlp.exe")
                os.environ["PATH"] = os.path.dirname(found_path) + os.pathsep + os.environ.get("PATH", "")
                return found_path
    return "yt-dlp"


def find_ffmpeg_bin():
    found = shutil.which("ffmpeg") or shutil.which("ffmpeg.exe")
    if found:
        return found
    local_app = os.environ.get("LOCALAPPDATA", "")
    if local_app:
        for root, _, files in os.walk(os.path.join(local_app, "Microsoft", "WinGet", "Packages")):
            if "ffmpeg.exe" in files:
                found_path = os.path.join(root, "ffmpeg.exe")
                os.environ["PATH"] = os.path.dirname(found_path) + os.pathsep + os.environ.get("PATH", "")
                return found_path
    return "ffmpeg"


YTDL_BIN = find_ytdl_bin()
FFMPEG_BIN = find_ffmpeg_bin()


def format_duration(seconds):
    seconds = int(seconds or 0)
    hours = seconds // 3600
    minutes = (seconds % 3600) // 60
    secs = seconds % 60
    if hours > 0:
        return f"{hours:02d}:{minutes:02d}:{secs:02d}"
    return f"{minutes:02d}:{secs:02d}"


def format_filesize(bytes_val):
    if not bytes_val:
        return "~Dynamic"
    if bytes_val >= 1073741824:
        return f"{bytes_val / 1073741824:.1f} GB"
    if bytes_val >= 1048576:
        return f"{bytes_val / 1048576:.1f} MB"
    if bytes_val >= 1024:
        return f"{bytes_val / 1024:.0f} KB"
    return f"{bytes_val} B"


def sanitize_filename(title, ext="mp4"):
    if not title:
        return f"video.{ext}"
    # Remove characters forbidden in Windows / POSIX filenames: \ / : * ? " < > |
    cleaned = re.sub(r'[\\/*?:"<>|]', "", str(title))
    # Replace multiple whitespaces with single space
    cleaned = re.sub(r"\s+", " ", cleaned).strip(". ")
    if not cleaned:
        cleaned = "video"
    if len(cleaned) > 120:
        cleaned = cleaned[:120].strip(". ")
    return f"{cleaned}.{ext}"


class DevHandler(http.server.BaseHTTPRequestHandler):
    def address_string(self):
        # Prevent 2-second Windows DNS reverse-lookup freeze
        return self.client_address[0]

    def send_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self):
        self.send_response(200)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.end_headers()

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)

        # Health endpoint
        if parsed.path == "/health":
            return self.send_json(200, {"status": "online", "mode": "python-dev-runner"})

        # Fast async streaming pipe with On-The-Fly FFmpeg Audio Muxing
        if parsed.path == "/api/proxy":
            query = urllib.parse.parse_qs(parsed.query)
            target_url = query.get("url", [None])[0]
            format_id = query.get("format_id", [None])[0]
            filename = query.get("filename", ["download.mp4"])[0]
            is_audio = query.get("audio", ["0"])[0] == "1"
            dl_token = query.get("token", [None])[0]

            if not target_url:
                return self.send_json(400, {"success": False, "error": {"code": "MISSING_URL", "message": "Missing target url"}})

            base_name, user_ext = os.path.splitext(filename)
            ext = user_ext.lstrip(".") if user_ext else ("m4a" if is_audio else "mp4")
            safe_name = sanitize_filename(base_name, ext)
            ascii_name = re.sub(r'[^a-zA-Z0-9.\-_ ]', '_', safe_name).strip('. ') or f"video.{ext}"
            encoded_name = urllib.parse.quote(safe_name)
            disposition = f'attachment; filename="{ascii_name}"; filename*=UTF-8\'\'{encoded_name}'

            # Case 1: Audio Only Download
            if is_audio:
                try:
                    cmd = [
                        YTDL_BIN,
                        "--no-warnings",
                        "--no-playlist",
                        "--js-runtimes", "node",
                        "-f", format_id or "ba",
                        "-o", "-",
                        target_url,
                    ]
                    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=1024*1024)
                    self.send_response(200)
                    self.send_header("Content-Type", "audio/mp4" if safe_name.endswith(".m4a") else "audio/mpeg")
                    self.send_header("Content-Disposition", disposition)
                    self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
                    if dl_token:
                        self.send_header("Set-Cookie", f"dl_token_{dl_token}=1; Path=/; SameSite=Lax")
                    self.end_headers()
                    try:
                        shutil.copyfileobj(proc.stdout, self.wfile, length=256*1024)
                    finally:
                        try: proc.kill()
                        except Exception: pass
                    return
                except Exception as e:
                    return self.send_json(502, {"success": False, "error": {"code": "AUDIO_STREAM_FAILED", "message": str(e)}})

            # Case 2: Video Download -> Stream Copy Mux Video + Audio with FFmpeg
            # Solves: "video diputar ga jalan" (H.264 MP4) + "diputar ga keluar suara" (Audio AAC Included)
            try:
                match = YT_REGEX.match(target_url)
                vid = match.group(5) if match else None

                # 1. Instant cache lookup for video stream URL (0ms instead of 6s)
                v_url = STREAM_CACHE.get(vid, {}).get("formats", {}).get(format_id)
                if not v_url:
                    p_v = subprocess.run([YTDL_BIN, "--force-ipv4", "--js-runtimes", "node", "-g", "-f", format_id, target_url], capture_output=True, text=True, timeout=15)
                    v_url = p_v.stdout.strip().splitlines()[0] if p_v.returncode == 0 and p_v.stdout.strip() else None

                # 2. Instant cache lookup for AAC audio stream URL (0ms instead of 6s)
                a_url = STREAM_CACHE.get(vid, {}).get("best_audio")
                if not a_url:
                    p_a = subprocess.run([YTDL_BIN, "--force-ipv4", "--js-runtimes", "node", "-g", "-f", "bestaudio[ext=m4a]/ba[acodec^=mp4a]/ba", target_url], capture_output=True, text=True, timeout=15)
                    a_url = p_a.stdout.strip().splitlines()[0] if p_a.returncode == 0 and p_a.stdout.strip() else None

                if v_url and a_url:
                    # Stream copy muxing: 0% CPU, 0 disk storage, full H.264 + AAC sound!
                    cmd = [
                        FFMPEG_BIN,
                        "-y",
                        "-i", v_url,
                        "-i", a_url,
                        "-c", "copy",
                        "-movflags", "frag_keyframe+empty_moov+default_base_moof",
                        "-f", "mp4",
                        "-"
                    ]
                    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=1024*1024)
                    self.send_response(200)
                    self.send_header("Content-Type", "video/mp4")
                    self.send_header("Content-Disposition", disposition)
                    self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
                    if dl_token:
                        self.send_header("Set-Cookie", f"dl_token_{dl_token}=1; Path=/; SameSite=Lax")
                    self.end_headers()
                    try:
                        shutil.copyfileobj(proc.stdout, self.wfile, length=256*1024)
                    finally:
                        try: proc.kill()
                        except Exception: pass
                    return
                else:
                    # Fallback muxed stream via yt-dlp native pipe
                    cmd = [YTDL_BIN, "--no-warnings", "--js-runtimes", "node", "-f", f"{format_id}+ba/{format_id}+140/{format_id}", "-o", "-", target_url]
                    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=1024*1024)
                    self.send_response(200)
                    self.send_header("Content-Type", "video/mp4")
                    self.send_header("Content-Disposition", disposition)
                    self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
                    if dl_token:
                        self.send_header("Set-Cookie", f"dl_token_{dl_token}=1; Path=/; SameSite=Lax")
                    self.end_headers()
                    try:
                        shutil.copyfileobj(proc.stdout, self.wfile, length=256*1024)
                    finally:
                        try: proc.kill()
                        except Exception: pass
                    return
            except Exception as e:
                return self.send_json(502, {"success": False, "error": {"code": "MUX_FAILED", "message": str(e)}})

        # Static file serving
        file_path = parsed.path.lstrip("/")
        if not file_path:
            file_path = "index.html"

        full_path = os.path.join(PUBLIC_DIR, file_path)
        if os.path.isfile(full_path):
            content_type = "text/html; charset=utf-8"
            if full_path.endswith(".css"):
                content_type = "text/css"
            elif full_path.endswith(".js"):
                content_type = "application/javascript"

            with open(full_path, "rb") as f:
                data = f.read()

            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        else:
            self.send_error(404, "File Not Found")

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        content_len = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_len).decode("utf-8") if content_len > 0 else "{}"

        try:
            payload = json.loads(body)
        except Exception:
            return self.send_json(400, {"success": False, "error": {"code": "INVALID_JSON", "message": "Malformed body"}})

        # POST /api/info
        if parsed.path == "/api/info":
            raw_url = payload.get("url", "").strip()
            match = YT_REGEX.match(raw_url)
            if not match:
                return self.send_json(
                    400,
                    {
                        "success": False,
                        "error": {
                            "code": "INVALID_URL",
                            "message": "Please enter a valid YouTube, Shorts, Live, or youtu.be link.",
                        },
                    },
                )

            video_id = match.group(5)
            clean_url = f"https://www.youtube.com/watch?v={video_id}"

            # 15-Minute in-memory cache lookup (0.001s instant response on repeat or concurrent users)
            if video_id in INFO_CACHE:
                cached_time, cached_payload = INFO_CACHE[video_id]
                if time.time() - cached_time < 900:  # 15 mins TTL
                    sys.stdout.write(f"[CACHE HIT] video_id={video_id}\n")
                    sys.stdout.flush()
                    return self.send_json(200, cached_payload)

            sys.stdout.write(f"[CACHE MISS] video_id={video_id}\n")
            sys.stdout.flush()

            try:
                proc = subprocess.run(
                    [
                        YTDL_BIN,
                        "--dump-json",
                        "--no-warnings",
                        "--no-playlist",
                        "--skip-download",
                        "--force-ipv4",
                        "--js-runtimes",
                        "node",
                        clean_url,
                    ],
                    capture_output=True,
                    text=True,
                    timeout=25,
                    check=True,
                )
                data = json.loads(proc.stdout)
            except subprocess.TimeoutExpired:
                return self.send_json(504, {"success": False, "error": {"code": "TIMEOUT", "message": "yt-dlp timed out."}})
            except Exception as e:
                return self.send_json(500, {"success": False, "error": {"code": "EXTRACTION_FAILED", "message": str(e)}})

            duration = int(data.get("duration") or 0)
            raw_formats = data.get("formats", [])
            formats = []

            # Populate in-memory cache with direct URLs already extracted in dump-json
            vid = data.get("id", video_id)
            STREAM_CACHE[vid] = {
                "formats": {},
                "best_audio": None
            }
            best_audio_abr = -1
            for f in raw_formats:
                fid = str(f.get("format_id") or "")
                u = f.get("url")
                if u and fid:
                    STREAM_CACHE[vid]["formats"][fid] = u
                    vcodec = (f.get("vcodec") or "none").lower()
                    acodec = (f.get("acodec") or "none").lower()
                    ext = (f.get("ext") or "").lower()
                    abr = f.get("abr") or 0
                    if vcodec == "none" and acodec != "none" and (ext == "m4a" or "mp4a" in acodec):
                        if abr > best_audio_abr:
                            best_audio_abr = abr
                            STREAM_CACHE[vid]["best_audio"] = u

            if not STREAM_CACHE[vid]["best_audio"]:
                for f in raw_formats:
                    if (f.get("vcodec") or "none") == "none" and (f.get("acodec") or "none") != "none" and f.get("url"):
                        STREAM_CACHE[vid]["best_audio"] = f.get("url")
                        break

            # 1. Best Video per resolution (1080p, 720p, 480p, 360p, etc.)
            def codec_rank(vcodec):
                vc = (vcodec or "").lower()
                if "avc" in vc or "h264" in vc:
                    return 3  # H.264: Universal hardware decode & audio player compatibility
                elif "vp09" in vc or "vp9" in vc:
                    return 2  # VP9: Used for 2K/4K UHD
                elif "av01" in vc:
                    return 1  # AV1
                return 0

            best_videos = {}
            best_audios = []

            for f in raw_formats:
                protocol = f.get("protocol", "")
                if protocol == "mhtml":
                    continue

                fid = str(f.get("format_id") or "")
                if not fid:
                    continue

                vcodec = f.get("vcodec") or "none"
                acodec = f.get("acodec") or "none"
                ext = f.get("ext") or "mp4"
                height = f.get("height") or 0
                size = f.get("filesize") or f.get("filesize_approx") or 0

                is_audio_only = vcodec == "none" and acodec != "none"
                is_video_only = vcodec != "none" and acodec == "none"
                is_muxed = vcodec != "none" and acodec != "none"

                if (is_video_only or is_muxed) and height >= 144:
                    # Prefer native HTTPS direct streams over m3u8_native if available
                    is_hls = "m3u8" in protocol
                    new_rank = codec_rank(vcodec)
                    existing = best_videos.get(height)
                    existing_rank = codec_rank(existing.get("vcodec")) if existing else -1
                    existing_hls = existing.get("is_hls", False) if existing else False

                    # Select if:
                    # 1. First format for this height
                    # 2. Existing was HLS and new is non-HLS
                    # 3. Higher codec compatibility (H.264 rank 3 > VP9 rank 2 > AV1 rank 1)
                    # 4. Same codec, but higher bitrate / filesize
                    replace = False
                    if existing is None:
                        replace = True
                    elif existing_hls and not is_hls:
                        replace = True
                    elif not existing_hls and is_hls:
                        replace = False
                    elif new_rank > existing_rank:
                        replace = True
                    elif new_rank == existing_rank and size > (existing.get("filesize") or 0):
                        replace = True

                    if replace:
                        codec_name = "H.264" if new_rank == 3 else ("VP9" if new_rank == 2 else ("AV1" if new_rank == 1 else ext.upper()))
                        best_videos[height] = {
                            "format_id": fid,
                            "ext": "mp4" if new_rank == 3 or is_muxed else ext,
                            "resolution": f"{height}p",
                            "fps": int(f.get("fps") or 0) if f.get("fps") else None,
                            "filesize": size,
                            "filesize_formatted": format_filesize(size),
                            "format_note": f"{height}p [{codec_name} • Full Audio Muxed]" if is_video_only else f"{height}p [Direct Muxed]",
                            "is_audio_only": False,
                            "is_video_only": is_video_only,
                            "is_muxed": True,  # Will be delivered fully muxed with audio!
                            "quality_label": f"{height}p ({codec_name} + AUDIO)",
                            "vcodec": vcodec,
                            "is_hls": is_hls,
                        }
                elif is_audio_only:
                    abr = f.get("abr") or 0
                    best_audios.append({
                        "format_id": fid,
                        "ext": ext,
                        "resolution": "Audio Only",
                        "fps": None,
                        "filesize": size,
                        "filesize_formatted": format_filesize(size),
                        "format_note": f"Audio [{ext.upper()} {round(abr)}k]",
                        "is_audio_only": True,
                        "is_video_only": False,
                        "is_muxed": False,
                        "quality_label": f"AUDIO [{ext.upper()}]",
                        "abr": abr,
                    })

            # Sort videos highest resolution first (2160p, 1440p, 1080p, 720p, 480p, 360p...)
            for h in sorted(best_videos.keys(), reverse=True):
                formats.append(best_videos[h])

            # Add top audio options (highest bitrate M4A/AAC and WebM/OPUS)
            m4a_audios = sorted([a for a in best_audios if a["ext"] == "m4a"], key=lambda a: a.get("abr", 0), reverse=True)
            opus_audios = sorted([a for a in best_audios if a["ext"] != "m4a"], key=lambda a: a.get("abr", 0), reverse=True)
            if m4a_audios:
                formats.append(m4a_audios[0])
            if opus_audios:
                formats.append(opus_audios[0])

            response_payload = {
                "success": True,
                "data": {
                    "id": data.get("id", video_id),
                    "title": data.get("title", "Untitled Media"),
                    "thumbnail": data.get("thumbnail", ""),
                    "duration": duration,
                    "duration_formatted": format_duration(duration),
                    "uploader": data.get("uploader", "Unknown Creator"),
                    "formats": formats,
                },
            }
            INFO_CACHE[video_id] = (time.time(), response_payload)
            return self.send_json(200, response_payload)

        # POST /api/resolve
        if parsed.path == "/api/resolve":
            raw_url = payload.get("url", "").strip()
            format_id = payload.get("format_id", "").strip()
            is_audio = "audio" in format_id.lower() or any(k in format_id for k in ["139", "140", "249", "250", "251", "ba"])

            match = YT_REGEX.match(raw_url)
            vid = match.group(5) if match else None
            direct_url = STREAM_CACHE.get(vid, {}).get("formats", {}).get(format_id, "")

            if not direct_url:
                try:
                    proc = subprocess.run(
                        [
                            YTDL_BIN,
                            "--no-warnings",
                            "--no-playlist",
                            "--js-runtimes",
                            "node",
                            "-g",
                            "-f",
                            format_id,
                            raw_url,
                        ],
                        capture_output=True,
                        text=True,
                        timeout=15,
                    )
                    if proc.returncode == 0 and proc.stdout.strip():
                        direct_url = proc.stdout.strip().splitlines()[0]
                except Exception:
                    pass

            title = payload.get("title", "").strip()
            if not title and vid and vid in INFO_CACHE:
                title = INFO_CACHE[vid][1].get("data", {}).get("title", "")

            ext = "m4a" if is_audio else "mp4"
            filename = sanitize_filename(title or f"media_{format_id}", ext)
            encoded_url = urllib.parse.quote(raw_url, safe="")
            encoded_name = urllib.parse.quote(filename, safe="")
            audio_param = "&audio=1" if is_audio else ""
            proxy_url = f"/api/proxy?url={encoded_url}&format_id={format_id}{audio_param}&filename={encoded_name}"

            return self.send_json(
                200,
                {
                    "success": True,
                    "data": {
                        "direct_url": direct_url or proxy_url,
                        "proxy_url": proxy_url,
                        "format_id": format_id,
                        "filename": filename,
                    },
                },
            )

        self.send_error(404, "Endpoint not found")


if __name__ == "__main__":
    server = http.server.ThreadingHTTPServer(("0.0.0.0", PORT), DevHandler)
    print("=" * 60)
    print(f" YTDL-RAW LOCAL DEV SERVER RUNNING: http://localhost:{PORT}")
    print(f" Engine: {YTDL_BIN}")
    print(" Frontend: public/index.html (Swiss Grid Brutalism)")
    print("=" * 60)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopping dev server...")
