//! UU file-transfer RPC (FILE_DATA_CHANNEL) for the Linux desktop.
//! Wire shapes come from the official Mac host's SwiftProtobuf name maps and field
//! descriptors: analysis/tools-fix-20261013/server-field-maps.json and server-fieldmd.json.
//! Message 21 RpcRequest{1 header{1 request_id}, 8 FileTransferFtpRequest};
//! Message 22 RpcResponse{1 header, 4 FileTransferFtpResponse}.
//! Logs carry counts and sizes only, never file names, paths or contents.

use crate::protocol::{decode_wire, WireField, WireValue};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// Proven values: an 18 MB download moved 505 KB/s with every block acknowledged. 128 KiB x 64
/// (8 MiB in flight) lost block acknowledgements instead of going faster, which truncated files,
/// so these stay the default and both are overridable to probe the ceiling without a rebuild.
const DOWNLOAD_BLOCK: usize = 32 * 1024;
const DOWNLOAD_WINDOW: usize = 16;

/// One environment override, ignored unless it is a positive number.
fn env_size(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).filter(|v| *v > 0).unwrap_or(default)
}

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

/// Field numbers of a decoded message, schema only — never values, names or paths.
fn numbers(fields: &[WireField]) -> String {
    fields.iter().map(|f| f.number.to_string()).collect::<Vec<_>>().join(" ")
}

/// Record a message shape once per distinct shape, capped. The 4.44.1 viewer drives file
/// transfers with messages this host has no handler for (#208), so the shapes are the
/// evidence for what it expects; the payloads are not ours to keep.
pub fn note_shape(what: &str, fields: &[WireField]) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let shape = format!("{what} [{}]", numbers(fields));
    let seen = SEEN.get_or_init(Default::default);
    let mut seen = seen.lock().unwrap();
    if seen.len() < 64 && seen.insert(shape.clone()) { log::info!("File transfer shape: {shape}"); }
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
    // Official Mac 4.46.0 UURemoteServer (x86_64, 0x10019f8a5–0x10019f9f7)
    // resolves :/Default to its receive directory. iOS quick upload sends this alias;
    // our receive directory is HOME, also reported in SystemMetrics/RemoteDownloadPath.
    if p.trim_end_matches('/') == ":/Default" { return home(); }
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

/// GvPb_FileEntry {1 entry_type, 2 name, 3 size, 4 modified_time, 5 full_path, 6 icon_type}
fn entry(kind: u64, name: &str, size: u64, modified: u64, full: &str) -> Enc {
    Enc::default().uint(1, kind).text(2, name).uint(3, size).uint(4, modified).text(5, full)
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
    out.push(entry(FT_DIR_DRIVE, "/", 0, 0, "/"));
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
    /// Like the official host's FileTransferDownloadResponseKey: replies are matched by
    /// (response case, task, file index, block id) because the viewer's RpcResponse header
    /// does not echo the host's request_id (observed 0).
    pending: Mutex<HashMap<(u32, u64, u64, u64), oneshot::Sender<Vec<u8>>>>,
    /// Our 1-based index of the file currently being sent, the third slot of a pending key. The
    /// viewer answers with its own 0-based index, which cannot be matched against ours, so the
    /// host records which file its blocks belong to. Zero between downloads.
    current_file: AtomicU64,
    uploads: Mutex<HashMap<u64, UploadTask>>,
    /// Downloads run one at a time, in request order (the viewer may ask for several at once).
    downloading: Arc<tokio::sync::Mutex<()>>,
    first_upload_block: Mutex<Option<u64>>,
    /// Download block size and how many blocks may be in flight, from the environment once per
    /// session so a test can drive a whole transfer with tiny blocks.
    block_size: usize,
    block_window: usize,
}

fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() }

impl FileSession {
    pub fn new(out: mpsc::UnboundedSender<Vec<u8>>, seq: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self { out, seq, next_request: AtomicU64::new(1 << 32), pending: Mutex::new(HashMap::new()),
            current_file: AtomicU64::new(0),
            uploads: Mutex::new(HashMap::new()), downloading: Arc::new(Default::default()), first_upload_block: Mutex::new(None),
            block_size: env_size("UU_FT_BLOCK", DOWNLOAD_BLOCK), block_window: env_size("UU_FT_WINDOW", DOWNLOAD_WINDOW) })
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
    /// Puts one request on the channel and hands back the slot its reply arrives in. Putting it on
    /// the channel and waiting for it are separate steps because a download must decide the order
    /// its blocks go out: on an ordered channel the wire order is the order of these calls, and a
    /// reply awaited straight away lets whichever task got polled first jump the queue.
    fn dispatch(&self, case: u32, body: Enc, key: (u32, u64, u64, u64)) -> oneshot::Receiver<Vec<u8>> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key, tx);
        let header = Enc::default().uint(1, id);
        self.send(21, Enc::default().msg(1, header).msg(8, Enc::default().msg(case, body)));
        rx
    }

    /// Waits for the reply to a dispatched request, dropping its slot if it never arrives.
    async fn await_reply(&self, key: (u32, u64, u64, u64), reply: oneshot::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        match tokio::time::timeout(std::time::Duration::from_secs(30), reply).await {
            Ok(Ok(v)) => Some(v),
            _ => { self.pending.lock().unwrap().remove(&key); None }
        }
    }

    /// The viewer's verdict on one dispatched block.
    async fn reap(&self, key: (u32, u64, u64, u64), reply: oneshot::Receiver<Vec<u8>>) -> Result<(), u64> {
        match self.await_reply(key, reply).await {
            Some(reply) => { let [err] = case_uints(&reply, 4, [3]); if err == 0 || err == FT_SUCCEEDED { Ok(()) } else { Err(err) } }
            None => Err(FT_IO),
        }
    }

    /// Host-initiated RpcRequest{1 header, 8 ftp{case}}; resolves with the response's ftp payload.
    async fn request(&self, case: u32, body: Enc, key: (u32, u64, u64, u64)) -> Option<Vec<u8>> {
        let reply = self.dispatch(case, body, key);
        self.await_reply(key, reply).await
    }

    /// Sends a request the viewer does not answer. Used for the file result notice: waiting for a
    /// response that never comes would stall the transfer by a full timeout per file.
    fn notify(&self, case: u32, body: Enc) {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let header = Enc::default().uint(1, id);
        self.send(21, Enc::default().msg(1, header).msg(8, Enc::default().msg(case, body)));
    }

    /// Key of an outstanding request. The viewer answers every case with a 0-based file_index
    /// while our own request carries a 1-based one, so the *viewer's* index can never be part of
    /// the key: doing so left all sixteen blocks of a download unmatched, and each stalled to the
    /// timeout. The third slot is ours instead - send_file restates current_file per file, and one
    /// download holds the `downloading` lock for its whole run, so at most one (file, block) pair
    /// per case is ever in flight. Block ids restart at the resume offset for every file, so
    /// without that slot two files of one task would answer to the same key.
    fn pending_key(case: u32, task: u64, file: u64, block: u64) -> (u32, u64, u64, u64) {
        (case, task, file, block)
    }

    /// Key of a FileTransferFtpResponse: 3 file_confirm{id}, 4 block_confirm{id, block_id}, 6 result{id}.
    /// A reply carries no usable file index, so it is keyed to whichever file we are sending.
    fn response_key(&self, ftp: &[u8]) -> Option<(u32, u64, u64, u64)> {
        let fields = decode_wire(ftp).ok()?;
        let case = fields.iter().find(|f| matches!(f.value, WireValue::Bytes(_)))?;
        let WireValue::Bytes(inner) = case.value else { return None };
        let inner = decode_wire(inner).ok()?;
        let id = TaskId::parse(&inner);
        let block = if case.number == 4 { get_uint(&inner, 2) } else { 0 };
        Some(Self::pending_key(case.number, id.task, self.current_file.load(Ordering::Relaxed), block))
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
                    match decode_wire(ftp) {
                        Ok(ftp) => self.on_request(request_id, &ftp),
                        Err(error) => log::info!("File transfer request undecodable: request_id={} bytes={} error={:?}", request_id, ftp.len(), error),
                    }
                }
                (22, WireValue::Bytes(rpc)) => {
                    let Ok(rpc) = decode_wire(rpc) else { continue };
                    let Some(ftp) = get_bytes(&rpc, 4) else { continue };
                    handled = true;
                    if let Ok(response) = decode_wire(ftp) { note_shape("response", &response); }
                    match self.response_key(ftp) {
                        Some(key) => match self.pending.lock().unwrap().remove(&key) {
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
        if case.number != 4 { note_shape(&format!("request case {}", case.number), &f); } // 4 streams thousands of identical blocks per upload
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
        let raw_target = get_text(f, 2);
        let target = normalize(&raw_target);
        log::info!("File transfer upload destination: default={} exists={}",
            raw_target.replace('\\', "/").trim_end_matches('/') == ":/Default", target.is_dir());
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
        // Newer viewers (Windows 4.44.1, #208) send only the task id here; an empty path must not
        // become "/" and walk the whole filesystem into a misleading permission error.
        if raw.trim().is_empty() {
            log::info!("File transfer send_request without path: task={} index={} fields={}", id.task, id.index, numbers(f));
            // #208 experiment: answer the id-only request with a well-formed empty listing and
            // watch what the viewer sends next. Off unless UU_FT_EMPTY_LIST is set; the viewer
            // never delivers a file list on any channel, so there is nothing to offer yet.
            if std::env::var_os("UU_FT_EMPTY_LIST").is_some_and(|v| v != "0") {
                self.respond(rid, 2, Enc::default().msg(1, id.enc()).uint(2, FT_SUCCEEDED).bytes(5, &list_data(&[])));
                return;
            }
            self.respond(rid, 2, Enc::default().msg(1, id.enc()).uint(2, FT_PROTOCOL));
            return;
        }
        let path = normalize(&raw);
        let collected = collect_download(&path).map_err(|e| io_error(&e));
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
        self.send(21, Enc::default().msg(1, Enc::default().uint(1, self.next_request.fetch_add(1, Ordering::Relaxed)))
            .msg(8, Enc::default().msg(9, Enc::default().msg(1, id.enc()).uint(2, if ok { FT_SUCCEEDED } else { FT_UNEXPECTED }))));
        log::info!("File transfer download task done: task={} files={} ok={}", task, files.len(), ok);
    }

    async fn download_files(self: &Arc<Self>, task: u64, files: &[(FileInfo, PathBuf)]) -> bool {
        // Block ids follow the viewer's own uploads (observed 0-based); file indexes are 1-based.
        let first_block = self.first_upload_block.lock().unwrap().unwrap_or(0);
        for (index, (info, path)) in files.iter().enumerate() {
            let index = index + 1;
            let id = TaskId { task, index: index as u64 };
            // Replies carry no file index we can trust, and block ids restart per file, so every
            // key from here on is attributed to this file until the next one starts.
            self.current_file.store(id.index, Ordering::Relaxed);
            let Some(confirm) = self.request(3, Enc::default().msg(1, id.enc()).uint(2, info.modified).uint(3, info.size), Self::pending_key(3, id.task, id.index, 0)).await else {
                log::warn!("File transfer download: no file_confirm task={} index={}", task, index); return false;
            };
            let [skip, err, resume] = case_uints(&confirm, 3, [2, 3, 4]);
            let skip = skip != 0;
            log::info!("File transfer download file_confirm: task={} index={} skip={} err={} resume={}", task, index, skip, err, resume);
            if err == FT_CANCELED { return false; }
            if skip || (err != FT_SUCCEEDED && err != 0) { continue; }
            let result = self.send_file(id, path, resume.min(info.size), first_block).await;
            let code = match result { Ok(()) => FT_SUCCEEDED, Err(c) => c };
            // The viewer acknowledges blocks and file_confirm but never answers this notice.
            self.notify(9, Enc::default().msg(1, id.enc()).uint(2, code));
            log::info!("File transfer download file done: task={} index={} bytes={} result={}", task, index, info.size, code);
            // A file that did not finish must fail the task: run_download reports the task result to
            // the viewer, and claiming success handed it a truncated file as a completed download.
            if code != FT_SUCCEEDED { return false; }
        }
        self.current_file.store(0, Ordering::Relaxed);
        true
    }

    async fn send_file(self: &Arc<Self>, id: TaskId, path: &Path, resume: u64, first_block: u64) -> Result<(), u64> {
        let mut file = std::fs::File::open(path).map_err(|e| io_error(&e))?;
        file.seek(SeekFrom::Start(resume)).map_err(|e| io_error(&e))?;
        let (block, window) = (self.block_size, self.block_window);
        let mut block_id = first_block + resume / block as u64;
        // Only this loop puts blocks on the ordered channel, so they leave in block order: awaiting
        // a reply used to happen in the task that sent its block, and whichever task got polled
        // first jumped the queue, which an 18 MB file arrived as - complete, acknowledged, contents
        // shuffled (result=1 and an APK that would not open).
        let (replied, mut replies) = mpsc::unbounded_channel();
        let mut inflight = 0usize;
        loop {
            // Fill a whole block: a short read in the middle would leave one block short and shift
            // every later block off the offset the viewer derives from its block id.
            let mut buf = vec![0u8; block];
            let mut filled = 0;
            while filled < block {
                match file.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(io_error(&e)),
                }
            }
            if filled == 0 { break; }
            buf.truncate(filled);
            // The replies come back shuffled, so a slot frees on whichever reply lands first.
            // Waiting for the oldest one instead idled the whole window behind a single slow block,
            // which cost close to half the throughput.
            while inflight >= window {
                let Some(verdict) = replies.recv().await else { return Err(FT_IO) };
                inflight -= 1;
                verdict?;
            }
            let key = Self::pending_key(4, id.task, id.index, block_id);
            let body = Enc::default().msg(1, id.enc()).uint(2, block_id).bytes(3, &buf);
            let reply = self.dispatch(4, body, key);
            let me = self.clone();
            let replied = replied.clone();
            inflight += 1;
            tokio::spawn(async move { let _ = replied.send(me.reap(key, reply).await); });
            block_id += 1;
        }
        while inflight > 0 {
            let Some(verdict) = replies.recv().await else { return Err(FT_IO) };
            inflight -= 1;
            verdict?;
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
    fn pending_key_separates_cases_tasks_files_and_blocks() {
        assert_ne!(FileSession::pending_key(3, 8, 1, 0), FileSession::pending_key(6, 8, 1, 0));
        assert_ne!(FileSession::pending_key(3, 8, 1, 0), FileSession::pending_key(3, 9, 1, 0));
        // Blocks of one file are confirmed concurrently, so case 4 must keep the block id.
        assert_ne!(FileSession::pending_key(4, 8, 1, 2), FileSession::pending_key(4, 8, 1, 3));
        assert_eq!(FileSession::pending_key(4, 8, 1, 2), FileSession::pending_key(4, 8, 1, 2));
        // Block ids restart for the next file of the same task, so only the file slot keeps the
        // two files of one task apart.
        assert_ne!(FileSession::pending_key(4, 8, 1, 0), FileSession::pending_key(4, 8, 2, 0));
        assert_ne!(FileSession::pending_key(3, 8, 1, 0), FileSession::pending_key(3, 8, 2, 0));
    }

    #[test]
    fn block_confirm_matches_the_key_send_file_waits_on() {
        // block_confirm{1 id, 2 block_id, 3 file_error} as the viewer actually sends it: with a
        // 0-based file_index, which is not the index our request carried, so the key takes the
        // file we are sending instead. Keying it on the reply's index left every block unmatched,
        // and a download stopped after its sixteen in-flight blocks and burned their 30s timeouts (#208).
        let id = TaskId { task: 8, index: 0 };
        let ack = Enc::default().msg(4, Enc::default().msg(1, id.enc()).uint(2, 7).uint(3, 0));
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        session.current_file.store(1, Ordering::Relaxed);
        assert_eq!(session.response_key(&ack.0).unwrap(), FileSession::pending_key(4, 8, 1, 7));
        // Between downloads nothing is matchable at all.
        session.current_file.store(0, Ordering::Relaxed);
        assert_eq!(session.response_key(&ack.0).unwrap(), FileSession::pending_key(4, 8, 0, 7));
    }
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
    fn ios_default_path_resolves_to_the_advertised_receive_directory() {
        for path in [":/Default", ":\\Default", ":/Default/"] {
            assert_eq!(normalize(path), home());
            assert!(!is_this_pc(path));
        }
        assert_eq!(normalize("/Default"), PathBuf::from("/Default"));
        assert_eq!(normalize("C:/Default"), PathBuf::from("/Default"));
        assert_eq!(normalize(":/Default-other"), PathBuf::from("/Default-other"));
    }
    #[test]
    fn ios_quick_upload_checks_conflicts_writes_and_completes_in_default_directory() {
        let name = format!(".uu-ios-test-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let dir = home().join(&name);
        std::fs::create_dir(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _cleanup = Cleanup(dir.clone());
        let rel = format!("{name}/x.bin");
        std::fs::write(dir.join("x.bin"), b"old").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        let call = |case: u32, body: Enc| {
            let rpc = Enc::default().msg(1, Enc::default().uint(1, 17)).msg(8, Enc::default().msg(case, body));
            assert!(s.handle(&decode_wire(&crate::initialization::envelope(1, 1, 21, &rpc.0)).unwrap()));
        };
        let response = |m: Vec<u8>, case: u32| {
            let outer = decode_wire(&m).unwrap();
            let rpc = decode_wire(get_bytes(&outer, 22).unwrap()).unwrap();
            assert_eq!(get_uint(&decode_wire(get_bytes(&rpc, 1).unwrap()).unwrap(), 1), 17);
            let ftp = decode_wire(get_bytes(&rpc, 4).unwrap()).unwrap();
            get_bytes(&ftp, case).unwrap().to_vec()
        };
        call(11, Enc::default().text(1, ":/Default").text(2, &rel));
        let exist = response(rx.try_recv().unwrap(), 8);
        let exist = decode_wire(&exist).unwrap();
        assert_eq!(get_text(&exist, 1), ":/Default");
        assert_eq!(get_uint(&decode_wire(get_bytes(&exist, 2).unwrap()).unwrap(), 2), 1);
        let id = TaskId { task: 991, index: 1 };
        let info = FileInfo { rel_path: rel, size: 6, modified: 0 };
        call(1, Enc::default().msg(1, id.enc()).text(2, ":/Default").bytes(6, &list_data(&[info])));
        let received = response(rx.try_recv().unwrap(), 1);
        assert_eq!(get_uint(&decode_wire(&received).unwrap(), 3), FT_SUCCEEDED);
        call(3, Enc::default().msg(1, id.enc()).uint(3, 6));
        let confirmed = response(rx.try_recv().unwrap(), 3);
        assert_eq!(get_uint(&decode_wire(&confirmed).unwrap(), 3), FT_SUCCEEDED);
        call(4, Enc::default().msg(1, id.enc()).bytes(3, b"abcdef"));
        let block = response(rx.try_recv().unwrap(), 4);
        assert_eq!(get_uint(&decode_wire(&block).unwrap(), 3), FT_SUCCEEDED);
        call(9, Enc::default().msg(1, id.enc()).uint(2, FT_SUCCEEDED));
        let done = response(rx.try_recv().unwrap(), 6);
        assert_eq!(get_uint(&decode_wire(&done).unwrap(), 2), FT_SUCCEEDED);
        assert_eq!(std::fs::read(dir.join("x.bin")).unwrap(), b"abcdef");
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

    #[tokio::test]
    async fn a_file_that_did_not_finish_fails_the_task() {
        // send_file gave up after losing a block acknowledgement, yet the task still ended with
        // FT_SUCCEEDED, so the viewer presented the truncated file as a completed download (#208).
        let (tx, _rx) = mpsc::unbounded_channel();
        let s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        let responder = s.clone();
        tokio::spawn(async move {
            // Accept the file once download_files is waiting for its file_confirm, so the run
            // reaches send_file and fails there the way a lost block acknowledgement does.
            for _ in 0..200 {
                let reply = responder.pending.lock().unwrap().remove(&FileSession::pending_key(3, 9, 1, 0));
                if let Some(reply) = reply {
                    let id = TaskId { task: 9, index: 1 };
                    let _ = reply.send(Enc::default().msg(3, Enc::default().msg(1, id.enc()).uint(3, FT_SUCCEEDED)).0);
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });
        let missing = std::env::temp_dir().join(format!("uu-missing-{}", std::process::id()));
        let files = vec![(FileInfo { rel_path: "gone.bin".into(), size: 4096, modified: 0 }, missing)];
        assert!(!s.download_files(9, &files).await);
    }

    #[tokio::test]
    async fn blocks_leave_in_block_order_whatever_order_replies_arrive() {
        // Each block used to be awaited in its own task, and whichever of those tasks got polled
        // first put the next block on the ordered channel. An 18 MB download arrived with every
        // block acknowledged and the right byte count, contents shuffled (#208).
        let dir = std::env::temp_dir().join(format!("uu-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.bin");
        std::fs::write(&file, b"0123456789").unwrap();
        let (tx, mut frames) = mpsc::unbounded_channel();
        let mut s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        { let session = Arc::get_mut(&mut s).unwrap(); session.block_size = 4; session.block_window = 3; }
        let responder = s.clone();
        tokio::spawn(async move {
            let id = TaskId { task: 9, index: 1 };
            let confirm = Enc::default().msg(3, Enc::default().msg(1, id.enc()).uint(3, FT_SUCCEEDED)).0;
            for _ in 0..200 {
                if let Some(reply) = responder.pending.lock().unwrap().remove(&FileSession::pending_key(3, 9, 1, 0)) {
                    let _ = reply.send(confirm); break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            for block in [0u64, 1, 2] {
                for _ in 0..200 {
                    if responder.pending.lock().unwrap().contains_key(&FileSession::pending_key(4, 9, 1, block)) { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
            for block in [2u64, 1, 0] {
                let reply = responder.pending.lock().unwrap().remove(&FileSession::pending_key(4, 9, 1, block));
                if let Some(reply) = reply {
                    let _ = reply.send(Enc::default().msg(4, Enc::default().msg(1, id.enc()).uint(3, 0)).0);
                }
            }
        });
        let files = vec![(FileInfo { rel_path: "f.bin".into(), size: 10, modified: 0 }, file)];
        assert!(s.download_files(9, &files).await);
        let mut order = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            let message = decode_wire(&frame).unwrap();
            let Some(rpc) = get_bytes(&message, 21) else { continue };
            let rpc = decode_wire(rpc).unwrap();
            let Some(ftp) = get_bytes(&rpc, 8) else { continue };
            let Ok(ftp) = decode_wire(ftp) else { continue };
            let Some(body) = get_bytes(&ftp, 4) else { continue };
            order.push(get_uint(&decode_wire(body).unwrap(), 2));
        }
        assert_eq!(order, vec![0, 1, 2]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn files_of_one_task_answer_to_their_own_reply_keys() {
        // send_file restarts block ids at the resume offset for every file, so the second file's
        // block 0 used to land on the first file's pending slot. A reply only carries the viewer's
        // own 0-based index, so the host has to remember which file it is sending.
        let dir = std::env::temp_dir().join(format!("uu-two-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.bin", "b.bin"] { std::fs::write(dir.join(name), b"0123456789").unwrap(); }
        let (tx, mut frames) = mpsc::unbounded_channel();
        let mut s = FileSession::new(tx, Arc::new(AtomicU64::new(1)));
        { let session = Arc::get_mut(&mut s).unwrap(); session.block_size = 4; session.block_window = 3; }
        let responder = s.clone();
        tokio::spawn(async move {
            for index in [1u64, 2] {
                let id = TaskId { task: 9, index };
                let mut confirm = None;
                for _ in 0..200 {
                    confirm = responder.pending.lock().unwrap().remove(&FileSession::pending_key(3, 9, index, 0));
                    if confirm.is_some() { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                let Some(confirm) = confirm else { panic!("file {index} never reached its file_confirm"); };
                let _ = confirm.send(Enc::default().msg(3, Enc::default().msg(1, id.enc()).uint(3, FT_SUCCEEDED)).0);
                for block in 0..3u64 {
                    let mut reply = None;
                    for _ in 0..200 {
                        reply = responder.pending.lock().unwrap().remove(&FileSession::pending_key(4, 9, index, block));
                        if reply.is_some() { break; }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    let Some(reply) = reply else { panic!("file {index} block {block} never reached the pending table"); };
                    let _ = reply.send(Enc::default().msg(4, Enc::default().msg(1, id.enc()).uint(3, 0)).0);
                }
            }
        });
        let files = ["a.bin", "b.bin"].iter()
            .map(|name| (FileInfo { rel_path: (*name).into(), size: 10, modified: 0 }, dir.join(name)))
            .collect::<Vec<_>>();
        assert!(s.download_files(9, &files).await);
        let mut sent = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            let message = decode_wire(&frame).unwrap();
            let Some(rpc) = get_bytes(&message, 21) else { continue };
            let rpc = decode_wire(rpc).unwrap();
            let Some(ftp) = get_bytes(&rpc, 8) else { continue };
            let Ok(ftp) = decode_wire(ftp) else { continue };
            let Some(body) = get_bytes(&ftp, 4) else { continue };
            let body = decode_wire(body).unwrap();
            sent.push((TaskId::parse(&body).index, get_uint(&body, 2)));
        }
        // Both files start their blocks at the same id; only the file slot tells them apart.
        assert_eq!(sent, vec![(1, 0), (1, 1), (1, 2), (2, 0), (2, 1), (2, 2)]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
