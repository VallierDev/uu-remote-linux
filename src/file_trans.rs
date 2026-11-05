//! UU file-transfer RPC (FILE_DATA_CHANNEL) for the Linux desktop.
//! Wire shapes come from the official Mac host's SwiftProtobuf name maps and field
//! descriptors: analysis/tools-fix-20261013/server-field-maps.json and server-fieldmd.json.
//! Message 21 RpcRequest{1 header{1 request_id}, 8 FileTransferFtpRequest};
//! Message 22 RpcResponse{1 header, 4 FileTransferFtpResponse}.
//! Logs carry counts and sizes only, never file names, paths or contents.

use crate::protocol::{decode_wire, WireField, WireValue};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

// FileTransferError
pub const FT_SUCCEEDED: u64 = 1;
const FT_UNEXPECTED: u64 = 2;
const FT_PROTOCOL: u64 = 3;
const FT_PERMISSION: u64 = 4;
const FT_DISK: u64 = 5;
const FT_IO: u64 = 6;
const FT_CANCELED: u64 = 7;
const FT_NOT_FOUND: u64 = 9;
// FileType
const FT_DIR: u64 = 0;
const FT_DIR_DRIVE: u64 = 3;
const FT_FILE: u64 = 4;
// FileOperationStrategy
const FOS_DUPLICATE: u64 = 2;
const FOS_SKIP: u64 = 3;
const FOS_RESUME: u64 = 4;

const DOWNLOAD_BLOCK: usize = 32 * 1024;
const DOWNLOAD_WINDOW: usize = 16;

// ---------- protobuf helpers ----------
fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; }
    out.push(v as u8);
}
#[derive(Default)]
struct Enc(Vec<u8>);
impl Enc {
    fn uint(mut self, field: u32, v: u64) -> Self {
        if v != 0 { varint(u64::from(field) << 3, &mut self.0); varint(v, &mut self.0); }
        self
    }
    fn boolean(self, field: u32, v: bool) -> Self { self.uint(field, v as u64) }
    fn bytes(mut self, field: u32, v: &[u8]) -> Self {
        varint((u64::from(field) << 3) | 2, &mut self.0); varint(v.len() as u64, &mut self.0);
        self.0.extend_from_slice(v); self
    }
    fn text(self, field: u32, v: &str) -> Self { if v.is_empty() { self } else { self.bytes(field, v.as_bytes()) } }
    fn msg(self, field: u32, v: Enc) -> Self { self.bytes(field, &v.0) }
}

fn get_uint(fields: &[WireField], n: u32) -> u64 {
    fields.iter().rev().find_map(|f| match (f.number == n, &f.value) { (true, WireValue::Varint(v)) => Some(*v), _ => None }).unwrap_or(0)
}
fn get_bytes<'a>(fields: &[WireField<'a>], n: u32) -> Option<&'a [u8]> {
    fields.iter().rev().find_map(|f| match (f.number == n, &f.value) { (true, WireValue::Bytes(v)) => Some(*v), _ => None })
}
fn get_all<'a>(fields: &[WireField<'a>], n: u32) -> Vec<&'a [u8]> {
    fields.iter().filter_map(|f| match (f.number == n, &f.value) { (true, WireValue::Bytes(v)) => Some(*v), _ => None }).collect()
}
fn get_text(fields: &[WireField], n: u32) -> String {
    get_bytes(fields, n).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
}

/// GvPb_TaskId {1 task_id, 2 file_index}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TaskId { pub task: u64, pub index: u64 }
impl TaskId {
    fn parse(fields: &[WireField]) -> Self {
        get_bytes(fields, 1).and_then(|b| decode_wire(b).ok())
            .map(|f| TaskId { task: get_uint(&f, 1), index: get_uint(&f, 2) }).unwrap_or_default()
    }
    fn enc(self) -> Enc { Enc::default().uint(1, self.task).uint(2, self.index) }
}

/// GvPb_FileInfo {1 rel_path, 2 size, 3 modified_time}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo { pub rel_path: String, pub size: u64, pub modified: u64 }
impl FileInfo {
    fn parse(b: &[u8]) -> Option<Self> {
        let f = decode_wire(b).ok()?;
        Some(Self { rel_path: get_text(&f, 1), size: get_uint(&f, 2), modified: get_uint(&f, 3) })
    }
    fn enc(&self) -> Enc { Enc::default().text(1, &self.rel_path).uint(2, self.size).uint(3, self.modified) }
}

/// Files may arrive inline (repeated field) or as list_data: a FileList {1 files}, compressed
/// when FF_FILE_TRANSFER_FTP_LIST_COMPRESS is negotiated (zlib; gzip/raw deflate tolerated).
fn parse_files(inline: Vec<&[u8]>, list_data: Option<&[u8]>) -> Vec<FileInfo> {
    let mut out: Vec<FileInfo> = inline.into_iter().filter_map(FileInfo::parse).collect();
    if out.is_empty() {
        if let Some(data) = list_data.filter(|d| !d.is_empty()) {
            let mut candidates = Vec::new();
            let mut buf = Vec::new();
            if flate2::read::ZlibDecoder::new(data).read_to_end(&mut buf).is_ok() { candidates.push(std::mem::take(&mut buf)); }
            if flate2::read::GzDecoder::new(data).read_to_end(&mut buf).is_ok() { candidates.push(std::mem::take(&mut buf)); }
            if flate2::read::DeflateDecoder::new(data).read_to_end(&mut buf).is_ok() { candidates.push(std::mem::take(&mut buf)); }
            candidates.push(data.to_vec());
            for c in candidates {
                if let Ok(f) = decode_wire(&c) {
                    let files: Vec<FileInfo> = get_all(&f, 1).into_iter().filter_map(FileInfo::parse).collect();
                    if !files.is_empty() { out = files; break; }
                }
            }
            if out.is_empty() {
                log::warn!("File transfer list_data not decoded: bytes={} magic={:02x?}", data.len(), &data[..data.len().min(2)]);
            }
        }
    }
    out
}

/// zlib(FileList{1 files}) for list_data.
fn list_data(files: &[FileInfo]) -> Vec<u8> {
    let mut list = Enc::default();
    for f in files { list = list.msg(1, f.enc()); }
    zlib(&list.0)
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

fn unix_secs(t: std::io::Result<std::time::SystemTime>) -> u64 {
    t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

fn io_error(e: &std::io::Error) -> u64 {
    match e.kind() {
        std::io::ErrorKind::NotFound => FT_NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => FT_PERMISSION,
        _ if e.raw_os_error() == Some(libc::ENOSPC) => FT_DISK,
        _ => FT_IO,
    }
}

/// Mirrors the official Mac path translation (UURemoteServer 0x1000b8100): backslashes
/// become "/", a leading "~" is the user's home directory (the viewer opens on "~/"),
/// anything else is rooted at "/".
pub fn normalize(path: &str) -> PathBuf {
    let p = path.replace('\\', "/");
    // The host registers as a PC, so the viewer builds drive paths ("C:/x", or ":/" when it
    // knows no drive letter). Linux has a single root: any drive prefix maps to "/".
    let p = match p.split_once(':') {
        Some((drive, rest)) if drive.len() <= 1 && drive.chars().all(|c| c.is_ascii_alphabetic())
            && (rest.is_empty() || rest.starts_with('/')) => rest.to_string(),
        _ => p,
    };
    let (mut out, p) = match p.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => (home(), rest.to_string()),
        _ => (PathBuf::from("/"), p),
    };
    for part in p.split('/') {
        match part { "" | "." => {}, ".." => { out.pop(); }, x => out.push(x) }
    }
    out
}

fn home() -> PathBuf { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")) }

fn windows_ftp_paths() -> bool { std::env::var("UU_FTP_WINDOWS_PATHS").is_ok_and(|v| v == "1") }

fn windows_ftp_path(path: &str) -> String {
    if path.starts_with('/') { format!("C:{}", path.replace('/', "\\")) } else { path.to_owned() }
}

/// GvPb_FileEntry {1 entry_type, 2 name, 3 size, 4 modified_time, 5 full_path, 6 icon_type}
fn entry(kind: u64, name: &str, size: u64, modified: u64, full: &str) -> Enc {
    // The host is advertised as a PC. Opt-in drive paths round-trip through normalize()
    // without changing the filesystem or the authenticated user's permissions.
    let full = if windows_ftp_paths() { windows_ftp_path(full) } else { full.to_owned() };
    Enc::default().uint(1, kind).text(2, name).uint(3, size).uint(4, modified).text(5, &full)
}

/// The viewer treats this host as a PC and opens "此电脑" by asking for the bare drive root
/// (":/" — no drive letter is known) or an empty path.
pub fn is_this_pc(path: &str) -> bool {
    let p = path.trim().replace('\\', "/");
    if p.is_empty() { return true; }
    let p = p.trim_end_matches('/');
    p.ends_with(':') && p.len() <= 2 && p[..p.len() - 1].chars().all(|c| c.is_ascii_alphabetic())
}

/// XDG user directory (e.g. XDG_DOCUMENTS_DIR="$HOME/文档") when it exists.
fn xdg_dir(key: &str) -> Option<PathBuf> {
    let home = home();
    let conf = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(conf.join("user-dirs.dirs")).ok()?;
    let value = text.lines().find_map(|l| l.trim().strip_prefix(&format!("{key}=")))?.trim().trim_matches('"').to_string();
    let path = PathBuf::from(value.replace("$HOME", &home.to_string_lossy()));
    (path != home && path.is_dir()).then_some(path)
}

/// "此电脑": like Windows, user folders first (Documents, Desktop, Downloads), then the home
/// folder, then the filesystem root as the only "drive".
fn this_pc() -> Vec<Enc> {
    let home = home();
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned());
    let modified = |p: &Path| unix_secs(std::fs::metadata(p).and_then(|m| m.modified()));
    let mut out = Vec::new();
    for key in ["XDG_DOCUMENTS_DIR", "XDG_DESKTOP_DIR", "XDG_DOWNLOAD_DIR"] {
        if let Some(dir) = xdg_dir(key) { out.push(entry(FT_DIR, &name(&dir), 0, modified(&dir), &dir.to_string_lossy())); }
    }
    out.push(entry(FT_DIR, &name(&home), 0, modified(&home), &home.to_string_lossy()));
    out.push(entry(FT_DIR_DRIVE, if windows_ftp_paths() { "C:" } else { "/" }, 0, 0, "/"));
    out
}

pub fn list_directory(path: &str) -> Result<Vec<Enc>, u64> {
    if is_this_pc(path) { return Ok(this_pc()); }
    let dir = normalize(path);
    let read = std::fs::read_dir(&dir).map_err(|e| io_error(&e))?;
    let mut list = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let full = dir.join(&name);
        // Follow links so linked folders stay navigable.
        let Ok(meta) = std::fs::metadata(&full).or_else(|_| item.metadata()) else { continue };
        let kind = if meta.is_dir() { FT_DIR } else { FT_FILE };
        let size = if meta.is_dir() { 0 } else { meta.len() };
        list.push((meta.is_dir(), name.to_lowercase(), entry(kind, &name, size, unix_secs(meta.modified()), &full.to_string_lossy())));
    }
    list.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(list.into_iter().map(|(_, _, e)| e).collect())
}

/// Recursive file list for a download request: (folder_name, files). For a folder, rel paths are
/// relative to it; for a single file, folder_name is empty and rel_path is the file name.
fn collect_download(root: &Path) -> std::io::Result<(String, Vec<(FileInfo, PathBuf)>)> {
    let meta = std::fs::metadata(root)?;
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut out = Vec::new();
    if !meta.is_dir() {
        out.push((FileInfo { rel_path: name.clone(), size: meta.len(), modified: unix_secs(meta.modified()) }, root.to_path_buf()));
        return Ok((String::new(), out));
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for item in std::fs::read_dir(&dir)?.flatten() {
            let p = item.path();
            let Ok(m) = std::fs::metadata(&p) else { continue };
            if m.is_dir() { stack.push(p); continue; }
            if !m.is_file() { continue; }
            let rel = p.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
            // Relative to folder_name, which the response carries separately.
            out.push((FileInfo { rel_path: rel, size: m.len(), modified: unix_secs(m.modified()) }, p));
        }
    }
    Ok((name, out))
}

fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() { return p.to_path_buf(); }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 1..10_000 {
        let c = p.with_file_name(format!("{stem} ({i}){ext}"));
        if !c.exists() { return c; }
    }
    p.to_path_buf()
}

struct UploadTask { target: PathBuf, folder: String, strategy: u64, files: Vec<FileInfo>, open: HashMap<u64, (std::fs::File, PathBuf, u64)> }

/// Per-connection file-transfer state. Outgoing messages are fully framed `Message`s.
pub struct FileSession {
    out: mpsc::UnboundedSender<Vec<u8>>,
    seq: Arc<AtomicU64>,
    next_request: AtomicU64,
    windows_mode: bool,
    /// Like the official host's FileTransferDownloadResponseKey: replies are matched by
    /// (response case, task, file index, block id) because the viewer's RpcResponse header
    /// does not echo the host's request_id (observed 0).
    pending: Mutex<HashMap<(u32, u64, u64, u64), oneshot::Sender<Vec<u8>>>>,
    uploads: Mutex<HashMap<u64, UploadTask>>,
    /// Downloads run one at a time, in request order (the viewer may ask for several at once).
    downloading: Arc<tokio::sync::Mutex<()>>,
    first_upload_block: Mutex<Option<u64>>,
}

fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() }

impl FileSession {
    pub fn new(out: mpsc::UnboundedSender<Vec<u8>>, seq: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self { out, seq, next_request: AtomicU64::new(1 << 32),
            windows_mode: std::env::var("UU_FTP_WINDOWS_MODE").is_ok_and(|v| v == "1"), pending: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()), downloading: Arc::new(Default::default()), first_upload_block: Mutex::new(None) })
    }

    fn send(&self, which: u32, body: Enc) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.out.send(crate::initialization::envelope(seq, now(), which, &body.0));
    }
    /// RpcResponse{1 header{1 request_id}, 4 ftp{case}}
    fn respond(&self, request_id: u64, case: u32, body: Enc) {
        let header = Enc::default().uint(1, request_id);
        self.send(22, Enc::default().msg(1, header).msg(4, Enc::default().msg(case, body)));
    }
    /// Host-initiated RpcRequest{1 header, 8 ftp{case}}; resolves with the response's ftp payload.
    async fn request(&self, case: u32, body: Enc, key: (u32, u64, u64, u64)) -> Option<Vec<u8>> {
        let id = self.request_id(key.1);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key, tx);
        let header = Enc::default().uint(1, id);
        self.send(21, Enc::default().msg(1, header).msg(8, Enc::default().msg(case, body)));
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(v)) => Some(v),
            _ => { self.pending.lock().unwrap().remove(&key); None }
        }
    }

    fn request_id(&self, task: u64) -> u64 {
        // Windows dispatches sender-initiated FTP requests by task ID, not a fresh RPC ID.
        // Interop reference: djkcyl/farwing, file_transfer/host/transfer.rs (official 4.41).
        if self.windows_mode { u64::from(task as u32) } else { self.next_request.fetch_add(1, Ordering::Relaxed) }
    }

    fn notify_complete(&self, id: TaskId, error: u64) {
        self.send(21, Enc::default().msg(1, Enc::default().uint(1, self.request_id(id.task)))
            .msg(8, Enc::default().msg(9, Enc::default().msg(1, id.enc()).uint(2, error))));
    }

    /// Key of a FileTransferFtpResponse: 3 file_confirm{id}, 4 block_confirm{id, block_id}, 6 result{id}.
    fn response_key(ftp: &[u8]) -> Option<(u32, u64, u64, u64)> {
        let fields = decode_wire(ftp).ok()?;
        let case = fields.iter().find(|f| matches!(f.value, WireValue::Bytes(_)))?;
        let WireValue::Bytes(inner) = case.value else { return None };
        let inner = decode_wire(inner).ok()?;
        let id = TaskId::parse(&inner);
        let block = if case.number == 4 { get_uint(&inner, 2) } else { 0 };
        Some((case.number, id.task, id.index, block))
    }

    fn pending_response(&self, key: (u32, u64, u64, u64)) -> Option<oneshot::Sender<Vec<u8>>> {
        let mut pending = self.pending.lock().unwrap();
        if let Some(tx) = pending.remove(&key) { return Some(tx); }
        // Windows confirmations may omit the file index. Accept only one outstanding match
        // for this task, response case and block; never guess across files or tasks.
        if !self.windows_mode || key.2 != 0 || !matches!(key.0, 3 | 4 | 6) { return None; }
        let matches: Vec<_> = pending.keys().filter(|k| k.0 == key.0 && k.1 == key.1 && k.3 == key.3).take(2).copied().collect();
        if matches.len() == 1 { pending.remove(&matches[0]) } else { None }
    }

    /// Handles one decoded Message. Returns true when it carried file-transfer RPC.
    pub fn handle(self: &Arc<Self>, message: &[WireField]) -> bool {
        let mut handled = false;
        for field in message {
            match (field.number, &field.value) {
                (21, WireValue::Bytes(rpc)) => {
                    let Ok(rpc) = decode_wire(rpc) else { continue };
                    let Some(ftp) = get_bytes(&rpc, 8) else { continue };
                    let request_id = get_bytes(&rpc, 1).and_then(|h| decode_wire(h).ok()).map(|h| get_uint(&h, 1)).unwrap_or(0);
                    handled = true;
                    if let Ok(ftp) = decode_wire(ftp) { self.on_request(request_id, &ftp); }
                }
                (22, WireValue::Bytes(rpc)) => {
                    let Ok(rpc) = decode_wire(rpc) else { continue };
                    let Some(ftp) = get_bytes(&rpc, 4) else { continue };
                    handled = true;
                    match Self::response_key(ftp) {
                        Some(key) => match self.pending_response(key) {
                            Some(tx) => { let _ = tx.send(ftp.to_vec()); }
                            None => log::info!("File transfer response without pending request: case={} task={} index={} block={}", key.0, key.1, key.2, key.3),
                        },
                        None => log::info!("File transfer response undecodable: bytes={}", ftp.len()),
                    }
                }
                _ => {}
            }
        }
        handled
    }

    fn on_request(self: &Arc<Self>, rid: u64, ftp: &[WireField]) {
        let Some(case) = ftp.iter().find(|f| matches!(f.value, WireValue::Bytes(_))) else {
            log::warn!("File transfer request without payload: fields={:?}", ftp.iter().map(|f| f.number).collect::<Vec<_>>());
            return;
        };
        let WireValue::Bytes(body) = case.value else { return };
        let Ok(f) = decode_wire(body) else { log::warn!("File transfer request {} undecodable", case.number); return };
        match case.number {
            10 => self.read_dir(rid, &f),
            1 => self.receive_request(rid, &f),
            3 => self.file_ask(rid, &f),
            4 => self.file_block(rid, &f),
            9 => self.complete(rid, &f),
            2 => self.send_request(rid, &f),
            5 | 6 | 7 | 8 => self.file_operation(rid, case.number, &f),
            11 => self.file_exist(rid, &f),
            12 => { log::info!("File transfer clear_send_temp"); self.respond(rid, 5, Enc::default().uint(1, FT_SUCCEEDED)); }
            n => log::info!("File transfer request case {} not implemented: fields={:?}", n, f.iter().map(|x| x.number).collect::<Vec<_>>()),
        }
    }

    /// read_dir {1 id, 2 path} -> file_directory {1 id, 2 path, 3 entries, 4 file_error}
    fn read_dir(&self, rid: u64, f: &[WireField]) {
        let id = TaskId::parse(f);
        let requested = get_text(f, 2);
        let drive = requested.len() >= 2 && requested.as_bytes()[1] == b':' && requested.as_bytes()[0].is_ascii_alphabetic();
        log::info!("File transfer read_dir path shape: len={} drive_letter={} backslashes={} leading_slash={} exists={}",
            requested.chars().count(), drive, requested.matches('\\').count(), requested.starts_with('/'),
            !requested.is_empty() && normalize(&requested).is_dir());
        if !requested.is_empty() && requested.chars().count() <= 16 && !normalize(&requested).is_dir() {
            // Short unresolved names are viewer-side virtual locations; record them to map correctly.
            log::info!("File transfer read_dir unresolved short path: {:?}", requested);
        }
        // Like the official Mac reader: only the virtual root path lists roots; anything else
        // is read as-is (backslashes normalized) and errors are reported, never redirected.
        let path = requested;
        let mut body = id.enc();
        body = Enc::default().msg(1, body).text(2, &path);
        match list_directory(&path) {
            Ok(entries) => {
                // FF_FILE_TRANSFER_FTP_LIST_COMPRESS: the official host sends the listing as
                // zlib(DirectoryData{1 entries}) in dir_data; entries are kept for older viewers.
                let mut listing = Enc::default();
                for e in &entries { listing = listing.bytes(1, &e.0); }
                let compressed = zlib(&listing.0);
                // Both copies in one message: a 5600-entry folder is ~73 KB compressed but several hundred KB
                // inline, beyond the 256 KB SCTP message limit, and the reply was dropped. Keep the inline copy
                // only when the message still fits; current viewers read dir_data.
                let inline = listing.0.len() + compressed.len() < 200_000;
                log::info!("File transfer read_dir: request_id={} root={} entries={} dir_data_bytes={} inline={}", rid, path.is_empty(), entries.len(), compressed.len(), inline);
                if inline { for e in entries { body = body.msg(3, e); } }
                body = body.uint(4, FT_SUCCEEDED).bytes(5, &compressed);
            }
            Err(code) => { log::info!("File transfer read_dir failed: request_id={} error={}", rid, code); body = body.uint(4, code); }
        }
        self.respond(rid, 7, body);
    }

    /// rename{1 path,2 new_name} dir_create{1 path} remove_file{1 path} remove_dir{1 id,2 path,3 recursive}
    fn file_operation(&self, rid: u64, case: u32, f: &[WireField]) {
        let path_field = if case == 8 { 2 } else { 1 };
        let raw = get_text(f, path_field);
        let path = normalize(&raw);
        let result = match case {
            5 => { let new = get_text(f, 2); if new.contains('/') || new.contains('\\') || new.is_empty() { Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)) } else { std::fs::rename(&path, path.with_file_name(new)) } }
            6 => std::fs::create_dir_all(&path),
            7 => std::fs::remove_file(&path),
            // The viewer confirms with the user and sends remove_dir for any selected item without
            // the recursive flag (like Explorer): files are unlinked, folders removed with contents.
            _ => match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(e) => Err(e),
            },
        };
        let code = match &result { Ok(()) => FT_SUCCEEDED, Err(e) => io_error(e) };
        log::info!("File transfer operation: case={} result={} os_error={:?}", case, code, result.as_ref().err().and_then(|e| e.raw_os_error()));
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        if case == 8 {
            // Official removeDir reply (UURemoteServer "文件操作-removeDir 回包"): the response oneof is
            // `result` (field 6) = FileTransferResult{1 id, 2 file_error} echoing the request's TaskId,
            // not operation_result; the viewer reported 删除文件失败 for operation_result.
            self.respond(rid, 6, Enc::default().msg(1, TaskId::parse(f).enc()).uint(2, code));
            return;
        }
        self.respond(rid, 5, Enc::default().uint(1, code).text(2, &msg).text(3, &raw));
    }

    /// file_exist{1 path, 2 names} -> file_exist_response{1 path, 2 results{1 name,2 has_same,3 has_transfering}}
    fn file_exist(&self, rid: u64, f: &[WireField]) {
        let raw = get_text(f, 1);
        let dir = normalize(&raw);
        let mut body = Enc::default().text(1, &raw);
        let names = get_all(f, 2);
        let mut same = 0;
        for n in &names {
            let name = String::from_utf8_lossy(n);
            let exists = dir.join(name.as_ref()).exists();
            same += exists as usize;
            body = body.msg(2, Enc::default().text(1, &name).boolean(2, exists));
        }
        log::info!("File transfer file_exist: names={} existing={}", names.len(), same);
        self.respond(rid, 8, body);
    }

    /// Upload to this host. receive_request{1 id,2 path,3 files,4 strategy,5 folder_name,6 list_data,7 task_unique_id}
    fn receive_request(&self, rid: u64, f: &[WireField]) {
        let id = TaskId::parse(f);
        let target = normalize(&get_text(f, 2));
        let folder = get_text(f, 5);
        let strategy = get_uint(f, 4);
        let files = parse_files(get_all(f, 3), get_bytes(f, 6));
        let prefixed = files.iter().filter(|x| !folder.is_empty() && x.rel_path.replace('\\', "/").starts_with(&format!("{folder}/"))).count();
        log::info!("File transfer receive_request: task={} files={} bytes={} folder={} rel_prefixed={} strategy={} target_dir={}",
            id.task, files.len(), files.iter().map(|x| x.size).sum::<u64>(), !folder.is_empty(), prefixed, strategy, target.is_dir());
        let err = if !target.is_dir() { FT_NOT_FOUND } else { FT_SUCCEEDED };
        let mut body = Enc::default().msg(1, id.enc());
        for x in &files { body = body.msg(2, x.enc()); }
        body = body.uint(3, err);
        if err == FT_SUCCEEDED && !folder.is_empty() {
            // Like the official createEmptyFolderIfNeeded: a folder (possibly empty) is created even
            // when its file list is empty.
            let mut dir = target.clone();
            for part in folder.replace('\\', "/").split('/') { match part { "" | "." | ".." => {}, x => dir.push(x) } }
            if let Err(e) = std::fs::create_dir_all(&dir) { log::warn!("File transfer folder create failed: {}", io_error(&e)); }
        }
        if err == FT_SUCCEEDED {
            self.uploads.lock().unwrap().insert(id.task, UploadTask { target, folder, strategy, files, open: HashMap::new() });
        }
        self.respond(rid, 1, body);
    }

    /// TaskId.file_index is 1-based (observed: the first file of a task is index 1).
    fn file_slot(index: u64) -> Option<usize> { (index as usize).checked_sub(1) }

    fn upload_path(task: &UploadTask, index: u64) -> Option<PathBuf> {
        let info = task.files.get(Self::file_slot(index)?)?;
        let rel = info.rel_path.replace('\\', "/");
        let rel = if !task.folder.is_empty() && !rel.starts_with(&format!("{}/", task.folder)) { format!("{}/{}", task.folder, rel) } else { rel };
        let mut p = task.target.clone();
        for part in rel.split('/') { match part { "" | "." | ".." => {}, x => p.push(x) } }
        Some(p)
    }

    /// file_ask{1 id,2 last_modified,3 file_size} -> file_confirm{1 id,2 skip,3 err,4 resume_point}
    fn file_ask(&self, rid: u64, f: &[WireField]) {
        let id = TaskId::parse(f);
        let size = get_uint(f, 3);
        let mut uploads = self.uploads.lock().unwrap();
        let (skip, err, resume) = match uploads.get_mut(&id.task) {
            None => (false, FT_PROTOCOL, 0),
            Some(task) => match Self::upload_path(task, id.index) {
                None => (false, FT_PROTOCOL, 0),
                Some(mut path) => {
                    let existing = std::fs::metadata(&path).ok().filter(|m| m.is_file()).map(|m| m.len());
                    if existing.is_some() && task.strategy == FOS_SKIP { (true, FT_SUCCEEDED, 0) }
                    else {
                        let resume = if task.strategy == FOS_RESUME { existing.filter(|e| *e <= size).unwrap_or(0) } else { 0 };
                        if existing.is_some() && task.strategy == FOS_DUPLICATE { path = unique_path(&path); }
                        let opened = path.parent().map(std::fs::create_dir_all).unwrap_or(Ok(()))
                            .and_then(|_| std::fs::OpenOptions::new().create(true).write(true).truncate(resume == 0).open(&path))
                            .and_then(|mut file| { file.seek(SeekFrom::Start(resume))?; Ok(file) });
                        match opened {
                            Ok(file) => { task.open.insert(id.index, (file, path, resume)); (false, FT_SUCCEEDED, resume) }
                            Err(e) => (false, io_error(&e), 0),
                        }
                    }
                }
            },
        };
        log::info!("File transfer file_ask: task={} index={} size={} skip={} err={} resume={}", id.task, id.index, size, skip, err, resume);
        self.respond(rid, 3, Enc::default().msg(1, id.enc()).boolean(2, skip).uint(3, err).uint(4, resume));
    }

    /// file_block{1 id,2 block_id,3 data} -> block_confirm{1 id,2 block_id,3 err,4 block_len}
    fn file_block(&self, rid: u64, f: &[WireField]) {
        let id = TaskId::parse(f);
        let block = get_uint(f, 2);
        let data = get_bytes(f, 3).unwrap_or_default();
        {
            let mut first = self.first_upload_block.lock().unwrap();
            if first.is_none() { *first = Some(block); log::info!("File transfer first upload block: block_id={} len={}", block, data.len()); }
        }
        let err = match self.uploads.lock().unwrap().get_mut(&id.task).and_then(|t| t.open.get_mut(&id.index)) {
            None => FT_PROTOCOL,
            Some((file, _, written)) => match file.write_all(data) { Ok(()) => { *written += data.len() as u64; FT_SUCCEEDED } Err(e) => io_error(&e) },
        };
        if err != FT_SUCCEEDED { log::warn!("File transfer block failed: task={} index={} block={} err={}", id.task, id.index, block, err); }
        self.respond(rid, 4, Enc::default().msg(1, id.enc()).uint(2, block).uint(3, err).uint(4, data.len() as u64));
    }

    /// complete{1 id,2 error} -> result{1 id,2 file_error,3 err_msg}
    fn complete(&self, rid: u64, f: &[WireField]) {
        let id = TaskId::parse(f);
        let error = get_uint(f, 2);
        let mut uploads = self.uploads.lock().unwrap();
        let mut code = FT_SUCCEEDED;
        if let Some(task) = uploads.get_mut(&id.task) {
            if let Some((file, path, written)) = task.open.remove(&id.index) {
                let expected = Self::file_slot(id.index).and_then(|i| task.files.get(i)).map(|x| x.size).unwrap_or(written);
                drop(file);
                if error != FT_SUCCEEDED && error != 0 { let _ = std::fs::remove_file(&path); code = error; }
                else if written != expected { code = FT_IO; }
                log::info!("File transfer upload file done: task={} index={} bytes={} expected={} viewer_error={} result={}", id.task, id.index, written, expected, error, code);
            }
            if task.open.is_empty() && id.index as usize >= task.files.len() { uploads.remove(&id.task); }
        }
        self.respond(rid, 6, Enc::default().msg(1, id.enc()).uint(2, code));
    }

    /// Download from this host. send_request{1 id,2 path,3 list_data} -> send_response{1 id,2 err,3 files,4 folder_name}
    fn send_request(self: &Arc<Self>, rid: u64, f: &[WireField]) {
        let id = TaskId::parse(f);
        let raw = get_text(f, 2);
        let path = normalize(&raw);
        // An empty Windows selection must not become a recursive filesystem-root download.
        let collected = if raw.is_empty() { Err(FT_PROTOCOL) } else { collect_download(&path).map_err(|e| io_error(&e)) };
        let mut body = Enc::default().msg(1, id.enc());
        match collected {
            Ok((folder, files)) => {
                log::info!("File transfer send_request: task={} files={} bytes={} folder={}", id.task, files.len(), files.iter().map(|x| x.0.size).sum::<u64>(), !folder.is_empty());
                body = body.uint(2, FT_SUCCEEDED);
                for (info, _) in &files { body = body.msg(3, info.enc()); }
                // The viewer indexes this list by file_index; with LIST_COMPRESS it reads list_data.
                let infos: Vec<FileInfo> = files.iter().map(|(i, _)| i.clone()).collect();
                body = body.text(4, &folder).bytes(5, &list_data(&infos));
                self.respond(rid, 2, body);
                let me = self.clone();
                tokio::spawn(async move {
                    let _turn = me.downloading.clone().lock_owned().await;
                    me.run_download(id.task, files).await;
                });
            }
            Err(code) => {
                log::info!("File transfer send_request failed: task={} error={}", id.task, code);
                self.respond(rid, 2, body.uint(2, code));
            }
        }
    }

    async fn run_download(self: &Arc<Self>, task: u64, files: Vec<(FileInfo, PathBuf)>) {
        let ok = self.download_files(task, &files).await;
        // Official host (FileTransferDownloadCoordinator.startDownloadTransfer, 0x1001cd860): after the
        // last file it sends, without waiting, complete{id{task, file_index = -1}, error}. The viewer
        // treats an index past its list as the task end and marks the download accepted.
        let id = TaskId { task, index: u64::MAX }; // int32 -1, sign-extended varint
        self.notify_complete(id, if ok { FT_SUCCEEDED } else { FT_UNEXPECTED });
        log::info!("File transfer download task done: task={} files={} ok={}", task, files.len(), ok);
    }

    async fn download_files(self: &Arc<Self>, task: u64, files: &[(FileInfo, PathBuf)]) -> bool {
        // Block ids follow the viewer's own uploads (observed 0-based); file indexes are 1-based.
        let first_block = self.first_upload_block.lock().unwrap().unwrap_or(0);
        for (index, (info, path)) in files.iter().enumerate() {
            let index = index + 1;
            let id = TaskId { task, index: index as u64 };
            let Some(confirm) = self.request(3, Enc::default().msg(1, id.enc()).uint(2, info.modified).uint(3, info.size), (3, id.task, id.index, 0)).await else {
                log::warn!("File transfer download: no file_confirm task={} index={}", task, index); return false;
            };
            let [skip, err, resume] = case_uints(&confirm, 3, [2, 3, 4]);
            let skip = skip != 0;
            log::info!("File transfer download file_confirm: task={} index={} skip={} err={} resume={}", task, index, skip, err, resume);
            if err == FT_CANCELED { return false; }
            if err != FT_SUCCEEDED && err != 0 { return false; }
            if skip { continue; }
            let result = self.send_file(id, path, resume.min(info.size), first_block).await;
            let code = match result { Ok(()) => FT_SUCCEEDED, Err(c) => c };
            // Basic Windows FTP acknowledges each block, but completion is a notification.
            // Do not turn a fully acknowledged transfer into a 30-second final-RPC timeout.
            if self.windows_mode {
                self.notify_complete(id, code);
                log::info!("File transfer download file sent: task={} index={} bytes={} result={} blocks_confirmed={}", task, index, info.size, code, code == FT_SUCCEEDED);
                if code != FT_SUCCEEDED { return false; }
                continue;
            }
            let reply = self.request(9, Enc::default().msg(1, id.enc()).uint(2, code), (6, id.task, id.index, 0)).await;
            let confirmed = reply.is_some();
            let r = reply.map(|r| case_uints(&r, 6, [2])[0]).unwrap_or(0);
            log::info!("File transfer download file done: task={} index={} bytes={} result={} viewer_result={}", task, index, info.size, code, r);
            if code != FT_SUCCEEDED || !confirmed || (r != FT_SUCCEEDED && r != 0) { return false; }
        }
        true
    }

    async fn send_file(self: &Arc<Self>, id: TaskId, path: &Path, resume: u64, first_block: u64) -> Result<(), u64> {
        let mut file = std::fs::File::open(path).map_err(|e| io_error(&e))?;
        file.seek(SeekFrom::Start(resume)).map_err(|e| io_error(&e))?;
        // Basic Windows FTP numbers blocks from one, resetting after a resume; FTP2 is not advertised.
        let mut block_id = if self.windows_mode { 1 } else { first_block + resume / DOWNLOAD_BLOCK as u64 };
        let window = if self.windows_mode { 1 } else { DOWNLOAD_WINDOW };
        let mut inflight = tokio::task::JoinSet::new();
        loop {
            let mut buf = vec![0u8; DOWNLOAD_BLOCK];
            let n = file.read(&mut buf).map_err(|e| io_error(&e))?;
            if n == 0 { break; }
            buf.truncate(n);
            while inflight.len() >= window {
                if let Some(Ok(code)) = inflight.join_next().await { if code != FT_SUCCEEDED { return Err(code); } }
            }
            let me = self.clone();
            let body = Enc::default().msg(1, id.enc()).uint(2, block_id).bytes(3, &buf);
            let key = (4, id.task, id.index, block_id);
            inflight.spawn(async move {
                match me.request(4, body, key).await {
                    Some(r) => { let [e] = case_uints(&r, 4, [3]); if e == 0 { FT_SUCCEEDED } else { e } }
                    None => FT_IO,
                }
            });
            block_id += 1;
        }
        while let Some(r) = inflight.join_next().await {
            if let Ok(code) = r { if code != FT_SUCCEEDED { return Err(code); } }
        }
        Ok(())
    }
}

/// Reads varint fields of one FileTransferFtpResponse case (0 when absent).
fn case_uints<const N: usize>(ftp: &[u8], case: u32, numbers: [u32; N]) -> [u64; N] {
    let fields = decode_wire(ftp).ok().and_then(|f| get_bytes(&f, case)).and_then(|b| decode_wire(b).ok()).unwrap_or_default();
    numbers.map(|n| get_uint(&fields, n))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_windows_and_relative_paths() {
        assert_eq!(normalize("C:\\..\\home\\a\\.\\b"), PathBuf::from("/home/a/b"));
        assert_eq!(normalize("/tmp//x/../y"), PathBuf::from("/tmp/y"));
        assert_eq!(normalize("~/"), home());
        assert_eq!(normalize("~\\a"), home().join("a"));
        assert_eq!(normalize("~x"), PathBuf::from("/~x"));
        assert_eq!(normalize(":/"), PathBuf::from("/"));
        assert_eq!(normalize("C:\\home\\a"), PathBuf::from("/home/a"));
        assert_eq!(normalize(":/home/a"), PathBuf::from("/home/a"));
        assert!(is_this_pc(":/") && is_this_pc("") && is_this_pc("C:\\") && is_this_pc("c:"));
        assert!(!is_this_pc("/") && !is_this_pc(":/home") && !is_this_pc("ab:/"));
        assert_eq!(normalize("/tmp/a:b"), PathBuf::from("/tmp/a:b"));
    }
    #[test]
    fn windows_ftp_paths_round_trip_without_changing_targets() {
        assert_eq!(windows_ftp_path("/"), "C:\\");
        assert_eq!(windows_ftp_path("/tmp/a b.txt"), "C:\\tmp\\a b.txt");
        for path in ["/", "/tmp/a b.txt", "/tmp/a:b", "/tmp/\u{6587}\u{6863}.md"] {
            assert_eq!(normalize(&windows_ftp_path(path)), PathBuf::from(path));
        }
        assert_eq!(windows_ftp_path("C:\\tmp\\a.txt"), "C:\\tmp\\a.txt");
    }
    #[test]
    fn read_dir_round_trip_uses_official_field_numbers() {
        let dir = std::env::temp_dir().join(format!("uu-ft-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        let read = Enc::default().msg(1, TaskId { task: 7, index: 0 }.enc()).text(2, &dir.to_string_lossy());
        let rpc = Enc::default().msg(1, Enc::default().uint(1, 99)).msg(8, Enc::default().msg(10, read));
        let msg = crate::initialization::envelope(5, 6, 21, &rpc.0);
        assert!(s.handle(&decode_wire(&msg).unwrap()));
        let out = rx.try_recv().unwrap();
        let outer = decode_wire(&out).unwrap();
        let rsp = decode_wire(get_bytes(&outer, 22).unwrap()).unwrap();
        assert_eq!(get_uint(&decode_wire(get_bytes(&rsp, 1).unwrap()).unwrap(), 1), 99);
        let ftp = decode_wire(get_bytes(&rsp, 4).unwrap()).unwrap();
        let fd = decode_wire(get_bytes(&ftp, 7).unwrap()).unwrap();
        assert_eq!(get_uint(&fd, 4), FT_SUCCEEDED);
        let mut listing = Vec::new();
        flate2::read::ZlibDecoder::new(get_bytes(&fd, 5).unwrap()).read_to_end(&mut listing).unwrap();
        assert_eq!(get_all(&decode_wire(&listing).unwrap(), 1).len(), 2);
        let entries: Vec<_> = get_all(&fd, 3).into_iter().map(|e| decode_wire(e).unwrap()).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(get_text(&entries[0], 2), "sub");
        assert_eq!(get_uint(&entries[0], 1), FT_DIR);
        assert_eq!(get_text(&entries[1], 2), "a.txt");
        assert_eq!(get_uint(&entries[1], 1), FT_FILE);
        assert_eq!(get_uint(&entries[1], 3), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn list_data_round_trips_through_compression() {
        let files = vec![FileInfo { rel_path: "d/a.txt".into(), size: 3, modified: 9 }];
        assert_eq!(parse_files(vec![], Some(&list_data(&files))), files);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut plain = Enc::default(); plain = plain.msg(1, files[0].enc());
        gz.write_all(&plain.0).unwrap();
        assert_eq!(parse_files(vec![], Some(&gz.finish().unwrap())), files);
    }
    #[test]
    fn empty_download_request_is_not_a_root_download() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        let request = Enc::default().msg(1, TaskId { task: 42, index: 0 }.enc());
        s.send_request(7, &decode_wire(&request.0).unwrap());
        let reply = rx.try_recv().unwrap();
        let outer = decode_wire(&reply).unwrap();
        let rpc = decode_wire(get_bytes(&outer, 22).unwrap()).unwrap();
        let ftp = decode_wire(get_bytes(&rpc, 4).unwrap()).unwrap();
        let body = decode_wire(get_bytes(&ftp, 2).unwrap()).unwrap();
        assert_eq!(get_uint(&body, 2), FT_PROTOCOL);
        assert!(get_all(&body, 3).is_empty());
        assert!(rx.try_recv().is_err());
    }
    fn download_request(message: &[u8]) -> (u64, u32, Vec<u8>) {
        let outer = decode_wire(message).unwrap();
        let rpc = decode_wire(get_bytes(&outer, 21).unwrap()).unwrap();
        let header = decode_wire(get_bytes(&rpc, 1).unwrap()).unwrap();
        let ftp = decode_wire(get_bytes(&rpc, 8).unwrap()).unwrap();
        let case = ftp.iter().find(|f| matches!(f.value, WireValue::Bytes(_))).unwrap();
        (get_uint(&header, 1), case.number, get_bytes(&ftp, case.number).unwrap().to_vec())
    }
    fn download_reply(session: &Arc<FileSession>, case: u32, body: Enc) {
        let rpc = Enc::default().msg(1, Enc::default()).msg(4, Enc::default().msg(case, body));
        assert!(session.handle(&decode_wire(&crate::initialization::envelope(1, 1, 22, &rpc.0)).unwrap()));
    }
    async fn next_download(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap()
    }
    #[tokio::test]
    async fn windows_download_preserves_task_correlation_and_file_bytes() {
        let dir = std::env::temp_dir().join(format!("uu-down-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("download.bin");
        let data: Vec<u8> = (0..DOWNLOAD_BLOCK * 2 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        Arc::get_mut(&mut s).unwrap().windows_mode = true;
        let initial = Enc::default().msg(1, TaskId { task: 71, index: u64::MAX }.enc()).text(2, &path.to_string_lossy());
        s.send_request(99, &decode_wire(&initial.0).unwrap());
        let reply = next_download(&mut rx).await;
        let outer = decode_wire(&reply).unwrap();
        let rpc = decode_wire(get_bytes(&outer, 22).unwrap()).unwrap();
        assert_eq!(get_uint(&decode_wire(get_bytes(&rpc, 1).unwrap()).unwrap(), 1), 99);
        let mut blocks = std::collections::BTreeMap::new();
        let mut completed = false;
        loop {
            let message = next_download(&mut rx).await;
            let (rid, case, bytes) = download_request(&message);
            assert_eq!(rid, 71);
            let body = decode_wire(&bytes).unwrap();
            let id = TaskId::parse(&body);
            assert_eq!(id.task, 71);
            if case == 9 && id.index == u64::MAX {
                assert!(completed);
                assert_eq!(get_uint(&body, 2), FT_SUCCEEDED);
                break;
            }
            assert_eq!(id.index, 1);
            match case {
                3 => {
                    assert_eq!(get_uint(&body, 3), data.len() as u64);
                    let reply_id = TaskId { index: 0, ..id };
                    download_reply(&s, 3, Enc::default().msg(1, reply_id.enc()).uint(3, FT_SUCCEEDED));
                }
                4 => {
                    let block = get_uint(&body, 2);
                    assert!(block >= 1);
                    let chunk = get_bytes(&body, 3).unwrap().to_vec();
                    assert!(blocks.insert(block, chunk.clone()).is_none());
                    tokio::task::yield_now().await;
                    assert!(rx.try_recv().is_err());
                    let reply_id = TaskId { index: 0, ..id };
                    download_reply(&s, 4, Enc::default().msg(1, reply_id.enc()).uint(2, block).uint(3, FT_SUCCEEDED).uint(4, chunk.len() as u64));
                }
                9 => {
                    assert_eq!(get_uint(&body, 2), FT_SUCCEEDED);
                    completed = true;
                }
                _ => panic!("unexpected download request"),
            }
        }
        assert_eq!(blocks.into_values().flatten().collect::<Vec<u8>>(), data);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn zero_index_confirmation_must_be_unique_and_task_scoped() {
        let (out, _rx) = mpsc::unbounded_channel();
        let mut s = FileSession::new(out, Arc::new(AtomicU64::new(1)));
        Arc::get_mut(&mut s).unwrap().windows_mode = true;
        let (a, _) = oneshot::channel();
        let (b, _) = oneshot::channel();
        s.pending.lock().unwrap().insert((4, 71, 1, 1), a);
        s.pending.lock().unwrap().insert((4, 71, 2, 1), b);
        assert!(s.pending_response((4, 71, 0, 1)).is_none());
        assert!(s.pending_response((4, 72, 0, 1)).is_none());
        assert!(s.pending_response((4, 71, 0, 2)).is_none());
        assert_eq!(s.pending.lock().unwrap().len(), 2);
        assert!(s.pending_response((4, 71, 1, 1)).is_some());
        assert!(s.pending_response((4, 71, 0, 1)).is_some());
        assert!(s.pending.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn rejected_download_is_not_reported_successful() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        Arc::get_mut(&mut s).unwrap().windows_mode = true;
        let worker = s.clone();
        let task = tokio::spawn(async move {
            worker.run_download(72, vec![(FileInfo { rel_path: "x.bin".into(), size: 3, modified: 0 }, PathBuf::from("unused"))]).await;
        });
        let message = next_download(&mut rx).await;
        let (rid, case, body) = download_request(&message);
        assert_eq!((rid, case), (72, 3));
        let id = TaskId::parse(&decode_wire(&body).unwrap());
        download_reply(&s, 3, Enc::default().msg(1, id.enc()).uint(3, FT_PERMISSION));
        let message = next_download(&mut rx).await;
        let (rid, case, body) = download_request(&message);
        assert_eq!((rid, case), (72, 9));
        let fields = decode_wire(&body).unwrap();
        assert_eq!(TaskId::parse(&fields).index, u64::MAX);
        assert_eq!(get_uint(&fields, 2), FT_UNEXPECTED);
        task.await.unwrap();
    }
    #[test]
    fn upload_writes_blocks_and_confirms() {
        let dir = std::env::temp_dir().join(format!("uu-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        let id = TaskId { task: 3, index: 1 };
        let call = |case: u32, body: Enc| {
            let rpc = Enc::default().msg(1, Enc::default().uint(1, 1)).msg(8, Enc::default().msg(case, body));
            s.handle(&decode_wire(&crate::initialization::envelope(1, 1, 21, &rpc.0)).unwrap());
        };
        let info = FileInfo { rel_path: "x.bin".into(), size: 6, modified: 0 };
        call(1, Enc::default().msg(1, id.enc()).text(2, &dir.to_string_lossy()).msg(3, info.enc()));
        call(3, Enc::default().msg(1, id.enc()).uint(3, 6));
        call(4, Enc::default().msg(1, id.enc()).uint(2, 1).bytes(3, b"abc"));
        call(4, Enc::default().msg(1, id.enc()).uint(2, 2).bytes(3, b"def"));
        call(9, Enc::default().msg(1, id.enc()).uint(2, FT_SUCCEEDED));
        let mut cases = Vec::new();
        while let Ok(m) = rx.try_recv() {
            let o = decode_wire(&m).unwrap();
            let r = decode_wire(get_bytes(&o, 22).unwrap()).unwrap();
            let ftp = decode_wire(get_bytes(&r, 4).unwrap()).unwrap();
            cases.push(ftp[0].number);
        }
        assert_eq!(cases, vec![1, 3, 4, 4, 6]);
        assert_eq!(std::fs::read(dir.join("x.bin")).unwrap(), b"abcdef");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
