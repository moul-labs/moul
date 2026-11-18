//! Uploaded files: storing them once per content hash, deciding who may
//! read them, and the file values records hold. Port of the Go engine's
//! upload handler and `filegate`.
//!
//! A file has a public id (15 letters and digits) and a URL
//! `/storage/<id>/original.<ext>`. Its bytes live in a [`Blobs`] store under
//! their SHA-256, so two uploads of the same bytes are kept once. In the
//! browser that store is the `files/` folder in OPFS; the service worker
//! serves `/storage/…` by asking the engine (Phase 5).

use std::collections::{BTreeMap, HashMap};

use moul_store::schema::{Collection, Field, FieldType};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Body, Response};
use crate::engine::{Cx, derive_key};
use crate::{crypto, ids, records, rules, values};

/// Where file bytes are kept, by SHA-256 (lowercase hex).
///
/// Each call gets the engine's connection inside the request's transaction,
/// so a store that keeps bytes in SQLite writes them atomically with the
/// upload's row.
pub trait Blobs {
    /// Stores `data` under `hash`. Storing the same hash again is fine.
    fn put(&mut self, conn: &Connection, hash: &str, data: &[u8]) -> Result<(), String>;
    /// The bytes stored under `hash`.
    fn get(&self, conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>, String>;
    /// Removes `hash`. Removing a missing hash is fine.
    fn delete(&mut self, conn: &Connection, hash: &str) -> Result<(), String>;
}

/// Blobs kept in memory, for tests and private windows.
#[derive(Debug, Default, Clone)]
pub struct MemoryBlobs {
    files: HashMap<String, Vec<u8>>,
}

impl MemoryBlobs {
    /// An empty store.
    pub fn new() -> MemoryBlobs {
        MemoryBlobs::default()
    }

    /// How many blobs are stored.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl Blobs for MemoryBlobs {
    fn put(&mut self, _: &Connection, hash: &str, data: &[u8]) -> Result<(), String> {
        self.files.insert(hash.to_owned(), data.to_vec());
        Ok(())
    }

    fn get(&self, _: &Connection, hash: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.files.get(hash).cloned())
    }

    fn delete(&mut self, _: &Connection, hash: &str) -> Result<(), String> {
        self.files.remove(hash);
        Ok(())
    }
}

/// Blobs in the device store's `_blobs` table: durable wherever the store
/// is (OPFS in the browser), written in the same transaction as the upload.
/// Each device keeps its own copy; the bytes never enter the shared store.
#[derive(Debug, Default, Clone, Copy)]
pub struct StoreBlobs;

impl Blobs for StoreBlobs {
    fn put(&mut self, conn: &Connection, hash: &str, data: &[u8]) -> Result<(), String> {
        conn.execute(
            "INSERT OR IGNORE INTO device._blobs (hash, data) VALUES (?1, ?2)",
            params![hash, data],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn get(&self, conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>, String> {
        conn.query_row(
            "SELECT data FROM device._blobs WHERE hash = ?1",
            [hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    fn delete(&mut self, conn: &Connection, hash: &str) -> Result<(), String> {
        conn.execute("DELETE FROM device._blobs WHERE hash = ?1", [hash])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Blobs in a folder on disk, one file per hash.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct DiskBlobs {
    dir: std::path::PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
impl DiskBlobs {
    /// Keeps blobs in `dir`, creating it if needed.
    pub fn new(dir: impl Into<std::path::PathBuf>) -> std::io::Result<DiskBlobs> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(DiskBlobs { dir })
    }

    fn path(&self, hash: &str) -> Result<std::path::PathBuf, String> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("not a SHA-256: {hash:?}"));
        }
        Ok(self.dir.join(hash))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Blobs for DiskBlobs {
    fn put(&mut self, _: &Connection, hash: &str, data: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let path = self.path(hash)?;
        // A file of the right length was written whole: it is renamed into
        // place only after its bytes are on disk.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == data.len() as u64) {
            return Ok(());
        }
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        // Make the rename durable too, where the platform allows syncing a
        // folder.
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn get(&self, _: &Connection, hash: &str) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(self.path(hash)?) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn delete(&mut self, _: &Connection, hash: &str) -> Result<(), String> {
        match std::fs::remove_file(self.path(hash)?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// The content security policy uploads are served with: never as a page.
pub const STORAGE_CSP: &str = "default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'; sandbox";

/// The largest image accepted, in pixels, as the Go engine's limit.
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;

fn valid_id(id: &str) -> bool {
    id.len() == 15 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A file name cleaned for URLs: Go's `SlugifyFilename`.
pub fn slugify_filename(name: &str) -> String {
    if name.is_empty() {
        return "file".into();
    }
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 || name.len() > 1 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let clean_ext: String = if ext.len() > 1 {
        let rest: String = ext[1..]
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .collect();
        format!(".{rest}")
    } else {
        String::new()
    };
    let mut out = String::new();
    let mut hyphen = false;
    for ch in base.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            out.push(ch);
            hyphen = false;
        } else if !hyphen && !out.is_empty() {
            out.push('-');
            hyphen = true;
        }
    }
    let out = out.trim_matches('-').to_owned();
    let out = if out.is_empty() {
        "file".to_owned()
    } else {
        out
    };
    if clean_ext == "." {
        out
    } else {
        out + &clean_ext
    }
}

/// The media type of `data` by its first bytes, a subset of Go's
/// `http.DetectContentType`: images, audio, video, PDF, archives, HTML,
/// XML, plain text, else `application/octet-stream`.
pub fn sniff(data: &[u8]) -> &'static str {
    let starts = |p: &[u8]| data.starts_with(p);
    if starts(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if starts(b"\xff\xd8\xff") {
        return "image/jpeg";
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return "image/gif";
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "image/webp";
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return "audio/wave";
    }
    if starts(b"BM") {
        return "image/bmp";
    }
    if starts(b"%PDF-") {
        return "application/pdf";
    }
    if starts(b"PK\x03\x04") {
        return "application/zip";
    }
    if starts(b"\x1f\x8b\x08") {
        return "application/x-gzip";
    }
    if starts(b"ID3") {
        return "audio/mpeg";
    }
    if starts(b"OggS") {
        return "application/ogg";
    }
    if starts(b"\x1aE\xdf\xa3") {
        return "video/webm";
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return "video/mp4";
    }
    if starts(b"wOFF") {
        return "font/woff";
    }
    if starts(b"wOF2") {
        return "font/woff2";
    }
    let text_start = data
        .iter()
        .position(|b| !matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        .unwrap_or(data.len());
    let head = &data[text_start..];
    let lower: Vec<u8> = head.iter().take(16).map(u8::to_ascii_lowercase).collect();
    for tag in [
        &b"<!doctype html"[..],
        b"<html",
        b"<head",
        b"<script",
        b"<iframe",
        b"<h1",
        b"<div",
        b"<font",
        b"<table",
        b"<a",
        b"<style",
        b"<title",
        b"<b",
        b"<body",
        b"<br",
        b"<p",
        b"<!--",
    ] {
        if lower.starts_with(tag)
            && lower
                .get(tag.len())
                .is_some_and(|b| *b == b' ' || *b == b'>')
        {
            return "text/html; charset=utf-8";
        }
    }
    if lower.starts_with(b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    let sample = &data[..data.len().min(512)];
    if sample
        .iter()
        .any(|&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | 0x1b))
    {
        return "application/octet-stream";
    }
    "text/plain; charset=utf-8"
}

fn is_active_type(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    base.contains("html")
        || base.contains("xml")
        || base.contains("svg")
        || base.contains("javascript")
        || base.contains("ecmascript")
        || base == "text/css"
}

const ACTIVE_EXTS: &[&str] = &[
    ".html", ".htm", ".xhtml", ".xht", ".shtml", ".mht", ".mhtml", ".svg", ".svgz", ".xml", ".xsl",
    ".xslt", ".js", ".mjs", ".css", ".swf",
];

const INLINE_EXTS: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".mp4", ".webm", ".mov", ".mp3", ".ogg",
    ".wav", ".m4a",
];

fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(i) => name[i..].to_ascii_lowercase(),
        None => String::new(),
    }
}

fn extension_for(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "application/zip" => ".zip",
        "video/mp4" => ".mp4",
        "audio/mpeg" => ".mp3",
        _ => "",
    }
}

fn type_for_extension(ext: &str) -> &'static str {
    match ext {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".avif" => "image/avif",
        ".pdf" => "application/pdf",
        ".txt" => "text/plain; charset=utf-8",
        ".csv" => "text/csv; charset=utf-8",
        ".json" => "application/json",
        ".mp4" => "video/mp4",
        ".webm" => "video/webm",
        ".mov" => "video/quicktime",
        ".mp3" => "audio/mpeg",
        ".ogg" => "audio/ogg",
        ".wav" => "audio/wav",
        ".m4a" => "audio/mp4",
        ".zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// The `file_allowed_types` setting: media types or wildcards such as
/// `image/*` members may upload. Empty allows every type that is not active
/// content.
fn allowed_types(cx: &Cx<'_>) -> ApiResult<Vec<String>> {
    let v: Option<String> = cx
        .conn
        .query_row(
            "SELECT value FROM writer._settings WHERE key = 'file_allowed_types'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.unwrap_or_default()
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect())
}

fn type_allowed(content_type: &str, allowed: &[String]) -> bool {
    allowed.is_empty()
        || allowed.iter().any(|a| {
            a == "*"
                || a == content_type
                || a.strip_suffix("/*")
                    .is_some_and(|p| content_type.starts_with(&format!("{p}/")))
        })
}

/// Width and height of a PNG, GIF or JPEG from its header.
fn image_size(data: &[u8]) -> Option<(u64, u64)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") && data.len() >= 24 {
        let w = u32::from_be_bytes(data[16..20].try_into().ok()?) as u64;
        let h = u32::from_be_bytes(data[20..24].try_into().ok()?) as u64;
        return Some((w, h));
    }
    if (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) && data.len() >= 10 {
        let w = u16::from_le_bytes([data[6], data[7]]) as u64;
        let h = u16::from_le_bytes([data[8], data[9]]) as u64;
        return Some((w, h));
    }
    if data.starts_with(b"\xff\xd8") {
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u64;
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u64;
                return Some((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// What signing a file URL needs: the engine's key, how long a URL lasts,
/// and the time. Requests and live deliveries both sign with it.
pub(crate) struct Signer<'a> {
    pub key: &'a [u8],
    pub ttl_ms: i64,
    pub now_ms: i64,
}

impl Signer<'_> {
    fn of<'a>(cx: &'a Cx<'_>) -> Signer<'a> {
        Signer {
            key: cx.key,
            ttl_ms: cx.config.file_url_ttl_ms,
            now_ms: cx.now_ms,
        }
    }

    /// Signs file `id` for a URL: `<expiry>.<mac>`, valid for one to two TTL
    /// windows, so URLs repeat within a window and can be cached.
    fn sign(&self, id: &str) -> String {
        let window = (self.ttl_ms / 1000).max(1);
        let exp = (self.now_ms.div_euclid(1000) / window + 2) * window;
        format!("{exp}.{}", self.mac(id, exp))
    }

    fn mac(&self, id: &str, exp: i64) -> String {
        let key = derive_key(self.key, "files");
        crypto::base64url(&crypto::hmac_sha256(&key, format!("{id}|{exp}").as_bytes()))
    }

    fn verify(&self, id: &str, sig: &str) -> bool {
        let Some((exp, m)) = sig.split_once('.') else {
            return false;
        };
        let Ok(exp) = exp.parse::<i64>() else {
            return false;
        };
        self.now_ms.div_euclid(1000) <= exp
            && crypto::constant_time_eq(m.as_bytes(), self.mac(id, exp).as_bytes())
    }

    fn sign_value(&self, value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(|v| self.sign_value(v)).collect()),
            Value::Object(obj) => {
                let mut out = obj.clone();
                let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
                if valid_id(id) {
                    let sig = self.sign(id);
                    if let Some(u) = obj.get("url").and_then(Value::as_str) {
                        out.insert("url".into(), json!(with_sig(u, &sig)));
                    }
                    if let Some(Value::Object(thumbs)) = obj.get("thumbs") {
                        let signed: Map<String, Value> = thumbs
                            .iter()
                            .map(|(k, v)| {
                                let s = v
                                    .as_str()
                                    .map(|u| json!(with_sig(u, &sig)))
                                    .unwrap_or(v.clone());
                                (k.clone(), s)
                            })
                            .collect();
                        out.insert("thumbs".into(), Value::Object(signed));
                    }
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
}

/// Signs file `id` for a URL within a request.
pub(crate) fn sign(cx: &Cx<'_>, id: &str) -> String {
    Signer::of(cx).sign(id)
}

fn verify(cx: &Cx<'_>, id: &str, sig: &str) -> bool {
    Signer::of(cx).verify(id, sig)
}

fn with_sig(url: &str, sig: &str) -> String {
    if url.starts_with("/storage/") {
        format!("{url}?sig={}", sig.replace('+', "%2B"))
    } else {
        url.to_owned()
    }
}

/// Signs the file URLs of a record a guest may not view, so the caller's
/// browser can load them. Port of Go's `signRecordFiles`.
pub(crate) fn sign_record(cx: &mut Cx<'_>, c: &Collection, rec: &mut values::Record) {
    sign_record_with(&Signer::of(cx), cx.conn, cx.catalog, cx.guard, c, rec);
}

/// [`sign_record`] outside a request, for live deliveries.
pub(crate) fn sign_record_with(
    signer: &Signer<'_>,
    conn: &Connection,
    catalog: &crate::engine::Catalog,
    guard: &crate::guard::SqlGuard,
    c: &Collection,
    rec: &mut values::Record,
) {
    let file_fields: Vec<&Field> = c
        .fields
        .iter()
        .filter(|f| f.kind == FieldType::File)
        .collect();
    if file_fields.is_empty() {
        return;
    }
    let public = rules::allows_db(
        conn,
        catalog,
        guard,
        &c.rules.view_rule,
        None,
        rec,
        None,
        signer.now_ms,
    )
    .unwrap_or(false);
    if public {
        return;
    }
    for f in file_fields {
        if let Some(v) = rec.get(&f.name).filter(|v| !v.is_null()) {
            let signed = signer.sign_value(v);
            rec.insert(f.name.clone(), signed);
        }
    }
}

/// `(id, canonical path)` of a storage URL, or an error in Go's words.
fn storage_path(raw: &str) -> Result<(String, String), String> {
    if raw.is_empty() {
        return Err("file url is required".into());
    }
    if raw.len() > 1024 {
        return Err("file url is too long".into());
    }
    let u = raw.split('?').next().unwrap_or("");
    let Some(rest) = u.strip_prefix("/storage/") else {
        return Err("file url must point to uploaded storage".into());
    };
    let bad = || "file url must point to uploaded storage".to_owned();
    let (id, name) = rest.split_once('/').ok_or_else(bad)?;
    if !valid_id(id) || name.is_empty() || ["/", "..", "%", "\\"].iter().any(|p| name.contains(p)) {
        return Err(bad());
    }
    Ok((id.to_owned(), format!("/storage/{id}/{name}")))
}

fn normalize_object(obj: &Map<String, Value>) -> Result<(Map<String, Value>, String), String> {
    const KEYS: &[&str] = &[
        "id",
        "filename",
        "url",
        "thumbhash",
        "thumbs",
        "size",
        "createdAt",
        "width",
        "height",
    ];
    let raw_url = obj.get("url").and_then(Value::as_str).unwrap_or("");
    let (url_id, path) = storage_path(raw_url)?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&url_id)
        .to_owned();
    if id != url_id {
        return Err(format!("file url does not belong to file {id:?}"));
    }
    let mut out = Map::new();
    for (k, v) in obj {
        if !KEYS.contains(&k.as_str()) {
            return Err(format!("unknown file attribute {k:?}"));
        }
        match k.as_str() {
            "url" | "id" => {}
            "thumbs" => {
                let Value::Object(thumbs) = v else {
                    return Err("thumbs must be an object".into());
                };
                let mut clean = Map::new();
                for (size, tv) in thumbs {
                    let tu = tv
                        .as_str()
                        .filter(|_| ["sm", "md", "lg"].contains(&size.as_str()));
                    let Some(tu) = tu else {
                        return Err(format!("invalid thumbnail {size:?}"));
                    };
                    let (tid, tpath) = storage_path(tu)?;
                    if tid != id {
                        return Err(format!("thumbnail {size:?} does not belong to file {id:?}"));
                    }
                    clean.insert(size.clone(), json!(tpath));
                }
                out.insert(k.clone(), Value::Object(clean));
            }
            "size" | "width" | "height" => {
                let n = v
                    .as_f64()
                    .filter(|n| *n >= 0.0 && *n <= (1u64 << 40) as f64);
                let Some(n) = n else {
                    return Err(format!("{k} must be a non-negative number"));
                };
                out.insert(k.clone(), values::number(n));
            }
            _ => {
                let s = v.as_str().filter(|s| s.len() <= 512);
                let Some(s) = s else {
                    return Err(format!("{k} must be a string of at most 512 bytes"));
                };
                out.insert(k.clone(), json!(s));
            }
        }
    }
    out.insert("id".into(), json!(id));
    out.insert("url".into(), json!(path));
    Ok((out, id))
}

/// Cleans a file field value and lists the file ids it names. Port of
/// Go's `filegate.NormalizeValue`.
fn normalize_value(
    value: &Value,
    max_items: usize,
) -> Result<(Option<Value>, Vec<String>), String> {
    let max_items = if max_items == 0 { 100 } else { max_items };
    match value {
        Value::Null => Ok((None, vec![])),
        Value::Object(o) => {
            let (obj, id) = normalize_object(o)?;
            Ok((Some(Value::Object(obj)), vec![id]))
        }
        Value::Array(items) => {
            if items.len() > max_items {
                return Err(format!("holds at most {max_items} files"));
            }
            let mut out = Vec::new();
            let mut ids = Vec::new();
            for item in items {
                let Value::Object(o) = item else {
                    return Err("must contain file objects".into());
                };
                let (obj, id) = normalize_object(o)?;
                out.push(Value::Object(obj));
                ids.push(id);
            }
            Ok((Some(Value::Array(out)), ids))
        }
        _ => Err("must be a file object or a list of file objects".into()),
    }
}

/// The file ids a stored file value names.
pub(crate) fn file_ids(value: &Value) -> Vec<String> {
    let one = |o: &Map<String, Value>| -> Option<String> {
        if let Some(id) = o.get("id").and_then(Value::as_str).filter(|i| valid_id(i)) {
            return Some(id.to_owned());
        }
        o.get("url")
            .and_then(Value::as_str)
            .and_then(|u| storage_path(u).ok())
            .map(|(id, _)| id)
    };
    match value {
        Value::Object(o) => one(o).into_iter().collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_object().and_then(one))
            .collect(),
        _ => vec![],
    }
}

/// What a file field stores for `val`: canonical URLs without signatures.
/// A non-admin may only attach files they uploaded or that this record
/// already holds in this field.
pub(crate) fn stored_value(
    cx: &mut Cx<'_>,
    c: &Collection,
    f: &Field,
    val: &Value,
    record_id: &str,
) -> ApiResult<Option<String>> {
    let max = f.max.map(|m| m as usize).unwrap_or(0);
    let (normalized, ids) = normalize_value(val, max)
        .map_err(|e| ApiError::bad_request(format!("Field {:?} {e}", f.name)))?;
    if !cx.who.is_admin() && !ids.is_empty() {
        let auth = cx.who.auth_map();
        let caller_id = auth
            .as_ref()
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let caller_moul = auth
            .as_ref()
            .and_then(|a| a.get("moul"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let held = if record_id.is_empty() {
            Vec::new()
        } else {
            record_refs(cx, &c.name, record_id)?
                .remove(&f.name)
                .unwrap_or_default()
        };
        for fid in &ids {
            if held.contains(fid) {
                continue;
            }
            let owner: Option<(String, String)> = cx
                .conn
                .query_row(
                    "SELECT ownerMoul, ownerId FROM main._files WHERE id = ?1",
                    [fid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let mine = owner.is_some_and(|(m, id)| {
                !caller_id.is_empty() && id == caller_id && m == caller_moul
            });
            if !mine {
                return Err(ApiError::bad_request(format!(
                    "Field {:?} references a file you did not upload",
                    f.name
                )));
            }
        }
    }
    Ok(normalized.map(|v| serde_json::to_string(&v).expect("json serialises")))
}

fn record_refs(cx: &Cx<'_>, moul: &str, record: &str) -> ApiResult<BTreeMap<String, Vec<String>>> {
    let mut stmt = cx
        .conn
        .prepare("SELECT field, fileId FROM main._file_refs WHERE moul = ?1 AND recordId = ?2")?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in stmt.query_map([moul, record], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (field, id) = row?;
        out.entry(field).or_default().push(id);
    }
    Ok(out)
}

/// Records which uploads the file fields of a just-written record name.
pub(crate) fn sync_refs(cx: &mut Cx<'_>, c: &Collection, rec: &values::Record) -> ApiResult<()> {
    let fields: Vec<&Field> = c
        .fields
        .iter()
        .filter(|f| f.kind == FieldType::File)
        .collect();
    if fields.is_empty() {
        return Ok(());
    }
    let id = rec.get("id").and_then(Value::as_str).unwrap_or("");
    cx.conn.execute(
        "DELETE FROM main._file_refs WHERE moul = ?1 AND recordId = ?2",
        [&c.name, id],
    )?;
    for f in fields {
        for fid in rec.get(&f.name).map(file_ids).unwrap_or_default() {
            cx.conn.execute(
                "INSERT OR IGNORE INTO main._file_refs (fileId, moul, recordId, field) VALUES (?1, ?2, ?3, ?4)",
                params![fid, c.name, id, f.name],
            )?;
        }
    }
    Ok(())
}

/// The answer an upload gave for file `id`, without a signature, for
/// replaying an upload request.
pub(crate) fn info_by_id(conn: &Connection, id: &str) -> Option<Value> {
    let row = records::query(
        conn,
        "SELECT id, name, size, createdAt FROM main._files WHERE id = :id",
        &[(":id".into(), rusqlite::types::Value::Text(id.into()))],
        None,
    )
    .ok()?
    .into_iter()
    .next()?;
    Some(file_info_unsigned(&row))
}

fn file_info_unsigned(row: &Map<String, Value>) -> Value {
    let id = row.get("id").and_then(Value::as_str).unwrap_or("");
    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let mut info = Map::new();
    info.insert("id".into(), json!(id));
    info.insert("filename".into(), json!(name));
    info.insert(
        "url".into(),
        json!(format!("/storage/{id}/original{}", extension(name))),
    );
    if let Some(size) = row.get("size").and_then(Value::as_i64).filter(|s| *s > 0) {
        info.insert("size".into(), json!(size));
    }
    if let Some(at) = row.get("createdAt").and_then(Value::as_str) {
        info.insert("createdAt".into(), json!(at));
    }
    Value::Object(info)
}

fn file_info(cx: &Cx<'_>, row: &Map<String, Value>, signed: bool) -> Value {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let path = format!("original{}", extension(name));
    let url = format!("/storage/{id}/{path}");
    let url = if signed {
        with_sig(&url, &sign(cx, &id))
    } else {
        url
    };
    let mut info = Map::new();
    info.insert("id".into(), json!(id));
    info.insert(
        "filename".into(),
        row.get("name").cloned().unwrap_or(json!("")),
    );
    info.insert("url".into(), json!(url));
    if let Some(size) = row.get("size").and_then(Value::as_i64).filter(|s| *s > 0) {
        info.insert("size".into(), json!(size));
    }
    if let Some(at) = row
        .get("createdAt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        info.insert("createdAt".into(), json!(at));
    }
    for key in ["width", "height"] {
        if let Some(n) = row.get(key).and_then(Value::as_i64).filter(|n| *n > 0) {
            info.insert(key.into(), json!(n));
        }
    }
    Value::Object(info)
}

fn file_path_name(name: &str, content_type: &str) -> (String, String) {
    let mut slug = slugify_filename(name);
    let mut ext = extension(&slug);
    if ext.is_empty() {
        ext = extension_for(content_type).to_owned();
        slug += &ext;
    }
    (slug, format!("original{ext}"))
}

/// `POST /api/upload`: a `multipart/form-data` body with a `file` part.
/// Members may not upload active content (HTML, SVG, XML, scripts).
pub(crate) fn upload(cx: &mut Cx<'_>) -> ApiResult {
    if !cx.who.is_admin() && cx.who.auth_map().is_none() {
        return Err(ApiError::unauthorized(
            "Authentication required (either valid JWT user or admin key)",
        ));
    }
    let file = match &cx.req.body {
        Body::Form { files, .. } => files
            .iter()
            .find(|(k, _)| k == "file")
            .map(|(_, f)| f.clone()),
        _ => None,
    };
    let Some(file) = file else {
        return Err(ApiError::bad_request(
            "Missing file in request body (form-data key: 'file')",
        ));
    };
    if file.data.len() > cx.config.max_upload_bytes {
        return Err(ApiError::new(413, "File is too large"));
    }
    let sniffed = sniff(&file.data);
    if !cx.who.is_admin() {
        let ext = extension(&slugify_filename(&file.name));
        if ACTIVE_EXTS.contains(&ext.as_str())
            || is_active_type(&file.content_type)
            || is_active_type(sniffed)
        {
            return Err(ApiError::bad_request("this file type is not allowed"));
        }
    }
    let content_type = sniffed.split(';').next().unwrap_or("").trim().to_owned();
    if !cx.who.is_admin() && !type_allowed(&content_type, &allowed_types(cx)?) {
        return Err(ApiError::bad_request(format!(
            "files of type {content_type} are not allowed"
        )));
    }
    if content_type.starts_with("image/")
        && let Some((w, h)) = image_size(&file.data)
        && w * h > MAX_IMAGE_PIXELS
    {
        return Err(ApiError::new(
            413,
            format!(
                "Image is too large to process (at most {} megapixels)",
                MAX_IMAGE_PIXELS / 1_000_000
            ),
        ));
    }
    let (name, path) = file_path_name(&file.name, &content_type);
    let hash = crypto::sha256_hex(&file.data);
    let id = ids::random_chars(cx.host, 15);
    let (owner_moul, owner_id) = match cx.who.user() {
        Some(u) => (u.collection.clone(), u.id.clone()),
        None => (String::new(), String::new()),
    };
    let existed: bool = cx
        .conn
        .query_row(
            "SELECT 1 FROM main._files WHERE hash = ?1 LIMIT 1",
            [&hash],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    cx.blobs
        .put(cx.conn, &hash, &file.data)
        .map_err(|e| ApiError::new(500, format!("Storage upload failed: {e}")))?;
    if !existed {
        cx.new_blobs.push(hash.clone());
    }
    let now = cx.now();
    cx.conn.execute(
        "INSERT INTO main._files (id, ownerMoul, ownerId, createdAt, hash, size, type, name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            owner_moul,
            owner_id,
            now,
            hash,
            file.data.len() as i64,
            content_type,
            name
        ],
    )?;
    cx.change = Some(crate::changes::ChangeInfo {
        collection: None,
        record: Some(id.clone()),
        fields: vec![],
        effects: Vec::new(),
    });
    debug_assert_eq!(path, format!("original{}", extension(&name)));
    let (width, height) = if content_type.starts_with("image/") {
        image_size(&file.data).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let row: Map<String, Value> = serde_json::from_value(json!({
        "id": id, "name": name, "size": file.data.len(), "createdAt": now,
        "width": width, "height": height,
    }))
    .expect("an object");
    Ok(Response::json(200, json!([file_info(cx, &row, true)])))
}

fn file_rows(cx: &Cx<'_>) -> ApiResult<Vec<Map<String, Value>>> {
    records::query(
        cx.conn,
        "SELECT id, name, size, createdAt FROM main._files ORDER BY createdAt, rowid",
        &[],
        None,
    )
}

/// `GET /api/upload` and `/api/files`: every upload, with signed URLs.
pub(crate) fn list(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let rows = file_rows(cx)?;
    let items: Vec<Value> = rows.iter().map(|r| file_info(cx, r, true)).collect();
    Ok(Response::json(200, Value::Array(items)))
}

/// The file id in a delete target, as Go's `ExtractFileID` finds it: an
/// id, `<id>/<name>`, `storage/<id>/<name>`, or a full URL.
fn extract_file_id(target: &str) -> String {
    let mut s = target.trim();
    if let Some(i) = s.find("://") {
        s = &s[i + 3..];
        s = s.find('/').map_or("", |j| &s[j + 1..]);
    }
    let s = s.split('?').next().unwrap_or("");
    let s = s.trim_start_matches('/');
    let s = s
        .strip_prefix("storage/")
        .unwrap_or(s)
        .trim_start_matches('/');
    let parts: Vec<&str> = s.split('/').collect();
    parts
        .iter()
        .find(|p| valid_id(p))
        .or(parts.first())
        .map(|p| (*p).to_owned())
        .unwrap_or_default()
}

/// `DELETE /api/files/<id or path>`
pub(crate) fn delete(cx: &mut Cx<'_>, target: &str) -> ApiResult {
    cx.require_admin()?;
    let id = extract_file_id(target);
    let hash: Option<String> = cx
        .conn
        .query_row("SELECT hash FROM main._files WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(hash) = hash else {
        return Err(ApiError::not_found("File not found"));
    };
    cx.conn
        .execute("DELETE FROM main._file_refs WHERE fileId = ?1", [&id])?;
    cx.conn
        .execute("DELETE FROM main._files WHERE id = ?1", [&id])?;
    if !hash.is_empty() {
        cx.dead_blobs.push(hash);
    }
    cx.change = Some(crate::changes::ChangeInfo {
        collection: None,
        record: Some(id.clone()),
        fields: vec![],
        effects: Vec::new(),
    });
    Ok(Response::json(
        200,
        json!({"message": "File deleted successfully", "id": id}),
    ))
}

/// Whether the caller may read upload `id`, and whether it is public.
fn access(cx: &mut Cx<'_>, id: &str) -> ApiResult<(bool, bool)> {
    let sig = cx.req.query_param("sig").to_owned();
    if !sig.is_empty() && verify(cx, id, &sig) {
        return Ok((true, false));
    }
    if cx.who.is_admin() {
        return Ok((true, false));
    }
    let owner: Option<(String, String)> = cx
        .conn
        .query_row(
            "SELECT ownerMoul, ownerId FROM main._files WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((owner_moul, owner_id)) = owner else {
        return Ok((false, false));
    };
    let auth = cx.who.auth_map();
    let is_uploader = auth.as_ref().is_some_and(|a| {
        !owner_id.is_empty()
            && a.get("id").and_then(Value::as_str) == Some(owner_id.as_str())
            && a.get("moul").and_then(Value::as_str) == Some(owner_moul.as_str())
    });
    let refs: Vec<(String, String)> = {
        let mut stmt = cx
            .conn
            .prepare("SELECT moul, recordId FROM main._file_refs WHERE fileId = ?1")?;
        stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let ctx = cx.request_context(None);
    let mut may_view = false;
    for (moul, record) in refs {
        let Some(c) = cx.collection(&moul)? else {
            continue;
        };
        let Some(rec) = records::load_normalized(cx, &c, &record)? else {
            continue;
        };
        // Public means any guest may view the record, judged without this
        // request's headers, so one caller cannot make a file public.
        if rules::allows(cx, &c.rules.view_rule, None, &rec, None).unwrap_or(false) {
            return Ok((true, true));
        }
        if auth.is_some()
            && !may_view
            && rules::allows(cx, &c.rules.view_rule, auth.as_ref(), &rec, Some(&ctx))
                .unwrap_or(false)
        {
            may_view = true;
        }
    }
    Ok((is_uploader || may_view, false))
}

/// `GET /storage/<id>/<name>`: the file, when the caller may read it. Every
/// refusal answers like a missing file.
pub(crate) fn serve(cx: &mut Cx<'_>, path: &str) -> ApiResult {
    let not_found = || ApiError::not_found("File not found");
    let path = path.split('?').next().unwrap_or("");
    let Some((id, name)) = path.trim_start_matches('/').split_once('/') else {
        return Err(not_found());
    };
    if !valid_id(id) || name.is_empty() || name.contains('/') || name.contains("..") {
        return Err(not_found());
    }
    let (allowed, public) = access(cx, id)?;
    if !allowed {
        return Err(not_found());
    }
    let hash: Option<String> = cx
        .conn
        .query_row("SELECT hash FROM main._files WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(hash) = hash.filter(|h| !h.is_empty()) else {
        return Err(not_found());
    };
    let Some(data) = cx
        .blobs
        .get(cx.conn, &hash)
        .map_err(|e| ApiError::new(500, e))?
    else {
        return Err(not_found());
    };
    let ext = extension(name);
    let mut r = Response::bytes(200, type_for_extension(&ext), data)
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("Content-Security-Policy", STORAGE_CSP)
        .with_header(
            "Cache-Control",
            if public {
                "public, max-age=300"
            } else {
                "private, max-age=3600"
            },
        );
    if !INLINE_EXTS.contains(&ext.as_str()) {
        r = r.with_header(
            "Content-Disposition",
            &format!("attachment; filename={name:?}"),
        );
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_go() {
        assert_eq!(
            slugify_filename("My Profile Photo (2026) & Info!.PNG"),
            "my-profile-photo-2026-info.png"
        );
        assert_eq!(slugify_filename(""), "file");
        assert_eq!(slugify_filename("a.tar.gz"), "a-tar.gz");
    }

    #[test]
    fn sniffs() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nxxxx"), "image/png");
        assert_eq!(sniff(b"  <html><body>"), "text/html; charset=utf-8");
        assert_eq!(sniff(b"<?xml version"), "text/xml; charset=utf-8");
        assert_eq!(sniff(b"hello"), "text/plain; charset=utf-8");
        assert_eq!(sniff(b"\x00\x01\x02"), "application/octet-stream");
    }

    #[test]
    fn file_values() {
        let v =
            json!({"url": "/storage/abcdefghijklmno/original.png?sig=1.x", "filename": "a.png"});
        let (n, ids) = normalize_value(&v, 0).unwrap();
        assert_eq!(ids, ["abcdefghijklmno"]);
        assert_eq!(n.unwrap()["url"], "/storage/abcdefghijklmno/original.png");
        assert!(normalize_value(&json!({"url": "https://evil/x"}), 0).is_err());
        assert!(normalize_value(&json!({"url": "/storage/abcdefghijklmno/a", "x": 1}), 0).is_err());
    }

    #[test]
    fn delete_targets_like_go() {
        for t in [
            "abcdefghijklmno",
            "abcdefghijklmno/original.png",
            "storage/abcdefghijklmno/original.png",
            "/storage/abcdefghijklmno/original.png?sig=1.x",
            "https://example.com/storage/abcdefghijklmno/original.png",
        ] {
            assert_eq!(extract_file_id(t), "abcdefghijklmno", "{t}");
        }
    }
}
