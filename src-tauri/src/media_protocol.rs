//! Range-aware, graph-scoped audio/video protocol. The requesting webview label
//! selects the current graph slot, and the core validates a top-level regular
//! asset on every request. Responses are capped to 1 MiB, so even a malformed or
//! range-less request can never make the app read a multi-gigabyte media file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{Manager, Runtime, UriSchemeContext};

const MAX_CHUNK: u64 = 1024 * 1024;

fn decode_path(path: &str) -> Option<String> {
    let bytes = path.strip_prefix('/').unwrap_or(path).as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn mime(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" | "mpeg" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

fn byte_range(value: Option<&header::HeaderValue>, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let raw = value?.to_str().ok()?.strip_prefix("bytes=")?;
    let first = raw.split(',').next()?;
    let (start, end) = first.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(len);
        Some((len.saturating_sub(suffix), len.saturating_sub(1)))
    } else {
        let start = start.parse::<u64>().ok()?;
        let end = if end.is_empty() {
            len.saturating_sub(1)
        } else {
            end.parse::<u64>().ok()?.min(len.saturating_sub(1))
        };
        (end >= start).then_some((start, end))
    }
}

fn response(status: StatusCode, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder().status(status).body(body).unwrap()
}

fn authority_name(authority: crate::state::AssetStreamAuthority) -> &'static str {
    match authority {
        crate::state::AssetStreamAuthority::Direct => "direct",
        crate::state::AssetStreamAuthority::Managed => "managed",
    }
}

fn diagnose_status(status: StatusCode, authority: &str) {
    if crate::debug::debug_enabled() {
        crate::debug::diag(format!(
            "media_protocol status={} authority={authority}",
            status.as_u16()
        ));
    }
}

pub(crate) fn respond<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let Some(bound_name) = decode_path(request.uri().path()) else {
        return response(StatusCode::BAD_REQUEST, Vec::new());
    };
    let Some((binding, name)) = bound_name.split_once('/') else {
        return response(StatusCode::BAD_REQUEST, Vec::new());
    };
    let Ok(binding) = binding.parse::<u64>() else {
        return response(StatusCode::BAD_REQUEST, Vec::new());
    };
    let state = ctx.app_handle().state::<crate::state::AppState>();
    let Ok(slot) = crate::state::slot_for_window(&state, ctx.webview_label()) else {
        diagnose_status(StatusCode::FORBIDDEN, "unbound");
        return response(StatusCode::FORBIDDEN, Vec::new());
    };
    respond_for_slot(&slot, binding, name, request)
}

fn respond_for_slot(
    slot: &crate::state::GraphSlot,
    binding: u64,
    name: &str,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    if slot.binding_generation != binding {
        diagnose_status(StatusCode::FORBIDDEN, "stale");
        return response(StatusCode::FORBIDDEN, Vec::new());
    }
    let resolved = match slot.asset_stream_path(name) {
        Ok(resolved) => resolved,
        Err(crate::state::AssetStreamError::AuthorityUnavailable(unavailable)) => {
            let authority = match unavailable {
                crate::state::AssetStreamUnavailable::DirectRetiring => "direct_retiring",
                crate::state::AssetStreamUnavailable::ManagedUnavailable => "managed_unavailable",
            };
            diagnose_status(StatusCode::FORBIDDEN, authority);
            return response(StatusCode::FORBIDDEN, Vec::new());
        }
        Err(crate::state::AssetStreamError::InvalidAsset { authority }) => {
            diagnose_status(StatusCode::NOT_FOUND, authority_name(authority));
            return response(StatusCode::NOT_FOUND, Vec::new());
        }
    };
    let authority = authority_name(resolved.authority);
    let Ok(mut file) = File::open(resolved.path) else {
        diagnose_status(StatusCode::NOT_FOUND, authority);
        return response(StatusCode::NOT_FOUND, Vec::new());
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        diagnose_status(StatusCode::INTERNAL_SERVER_ERROR, authority);
        return response(StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
    };
    if len == 0 {
        diagnose_status(StatusCode::OK, authority);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime(name))
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, 0)
            .body(Vec::new())
            .unwrap();
    }
    if request.method() == tauri::http::Method::HEAD {
        diagnose_status(StatusCode::OK, authority);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime(name))
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, len)
            .body(Vec::new())
            .unwrap();
    }
    let range_header = request.headers().get(header::RANGE);
    let requested = byte_range(range_header, len);
    if range_header.is_some() && requested.is_none() {
        diagnose_status(StatusCode::RANGE_NOT_SATISFIABLE, authority);
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Vec::new())
            .unwrap();
    }
    let start = requested.map(|range| range.0).unwrap_or(0);
    if start >= len && len != 0 {
        diagnose_status(StatusCode::RANGE_NOT_SATISFIABLE, authority);
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Vec::new())
            .unwrap();
    }
    let requested_count = requested
        .map(|(_, end)| end.saturating_add(1).saturating_sub(start))
        .unwrap_or_else(|| len.saturating_sub(start));
    let count = requested_count.min(MAX_CHUNK);
    let mut body = Vec::with_capacity(count as usize);
    if file.seek(SeekFrom::Start(start)).is_err()
        || file.by_ref().take(count).read_to_end(&mut body).is_err()
    {
        diagnose_status(StatusCode::INTERNAL_SERVER_ERROR, authority);
        return response(StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
    }
    let end = start.saturating_add(count).saturating_sub(1);
    let partial = request.headers().contains_key(header::RANGE) || count < len;
    let status = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    diagnose_status(status, authority);
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime(name))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, count);
    if partial {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }
    builder.body(body).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::GraphSlot;
    use tine_core::model::Graph;

    fn direct_slot() -> (tempfile::TempDir, GraphSlot) {
        let temp = tempfile::tempdir().unwrap();
        for relative in ["assets", "journals", "logseq", "pages"] {
            std::fs::create_dir_all(temp.path().join(relative)).unwrap();
        }
        std::fs::write(temp.path().join("logseq/config.edn"), b"{}\n").unwrap();
        std::fs::write(temp.path().join("assets/fixture.mp3"), b"abcdef").unwrap();
        std::fs::write(temp.path().join("assets/empty.wav"), b"").unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let slot = GraphSlot::new(Graph::open(&root), root);
        (temp, slot)
    }

    fn request(method: tauri::http::Method, range: Option<&str>) -> Request<Vec<u8>> {
        let mut builder = Request::builder()
            .uri("tine-media://localhost/fixture")
            .method(method);
        if let Some(range) = range {
            builder = builder.header(header::RANGE, range);
        }
        builder.body(Vec::new()).unwrap()
    }

    #[test]
    fn decodes_safe_percent_encoded_names() {
        assert_eq!(
            decode_path("/voice%20memo.wav").as_deref(),
            Some("voice memo.wav")
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("bytes=123-")), 500),
            Some((123, 499))
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("bytes=-20")), 500),
            Some((480, 499))
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("bytes=4-9")), 500),
            Some((4, 9))
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("bytes=0-1")), 500),
            Some((0, 1))
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("bytes=9-4")), 500),
            None
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("garbage")), 500),
            None
        );
        assert_eq!(
            byte_range(Some(&header::HeaderValue::from_static("bytes=0-")), 0),
            None
        );
    }

    #[test]
    fn direct_head_range_mime_zero_length_and_size_behavior_stays_stable() {
        let (temp, slot) = direct_slot();
        let generation = slot.binding_generation;

        let head = respond_for_slot(
            &slot,
            generation,
            "fixture.mp3",
            request(tauri::http::Method::HEAD, None),
        );
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::CONTENT_TYPE], "audio/mpeg");
        assert_eq!(head.headers()[header::CONTENT_LENGTH], "6");
        assert!(head.body().is_empty());

        let range = respond_for_slot(
            &slot,
            generation,
            "fixture.mp3",
            request(tauri::http::Method::GET, Some("bytes=1-2")),
        );
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(range.headers()[header::CONTENT_RANGE], "bytes 1-2/6");
        assert_eq!(range.headers()[header::CONTENT_LENGTH], "2");
        assert_eq!(range.body(), b"bc");

        let out_of_range = respond_for_slot(
            &slot,
            generation,
            "fixture.mp3",
            request(tauri::http::Method::GET, Some("bytes=99-")),
        );
        assert_eq!(out_of_range.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(out_of_range.headers()[header::CONTENT_RANGE], "bytes */6");

        let empty = respond_for_slot(
            &slot,
            generation,
            "empty.wav",
            request(tauri::http::Method::GET, Some("bytes=0-")),
        );
        assert_eq!(empty.status(), StatusCode::OK);
        assert_eq!(empty.headers()[header::CONTENT_TYPE], "audio/wav");
        assert_eq!(empty.headers()[header::CONTENT_LENGTH], "0");
        assert!(empty.body().is_empty());

        let oversized = vec![b'x'; usize::try_from(MAX_CHUNK).unwrap() + 3];
        std::fs::write(temp.path().join("assets/large.webm"), oversized).unwrap();
        let capped = respond_for_slot(
            &slot,
            generation,
            "large.webm",
            request(tauri::http::Method::GET, None),
        );
        assert_eq!(capped.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(capped.headers()[header::CONTENT_TYPE], "video/webm");
        assert_eq!(
            capped.headers()[header::CONTENT_LENGTH],
            MAX_CHUNK.to_string()
        );
        assert_eq!(capped.body().len(), usize::try_from(MAX_CHUNK).unwrap());
    }

    #[test]
    fn stale_binding_and_direct_containment_refuse_before_open() {
        let (temp, slot) = direct_slot();
        let stale = respond_for_slot(
            &slot,
            slot.binding_generation + 1,
            "fixture.mp3",
            request(tauri::http::Method::GET, None),
        );
        assert_eq!(stale.status(), StatusCode::FORBIDDEN);

        std::fs::write(temp.path().join("outside.mp3"), b"outside").unwrap();
        for name in ["../outside.mp3", "/outside.mp3"] {
            let refused = respond_for_slot(
                &slot,
                slot.binding_generation,
                name,
                request(tauri::http::Method::GET, None),
            );
            assert_eq!(refused.status(), StatusCode::NOT_FOUND, "name={name:?}");
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                temp.path().join("outside.mp3"),
                temp.path().join("assets/escape.mp3"),
            )
            .unwrap();
            let refused = respond_for_slot(
                &slot,
                slot.binding_generation,
                "escape.mp3",
                request(tauri::http::Method::GET, None),
            );
            assert_eq!(refused.status(), StatusCode::NOT_FOUND);
        }
    }
}
