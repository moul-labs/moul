//! Support for the Litestream compatibility suite.
//!
//! The suite proves that replicas work in both directions: Litestream 0.5.13
//! restores what Moul writes, and Moul restores and follows what Litestream
//! writes. Litestream runs through `main.go` in this folder, a small harness
//! over Litestream's own library and the Go LTX library it uses, built once
//! per test run with the local Go toolchain.
//!
//! Without Go the tests that need it print a note and pass, so a plain
//! `cargo test` works anywhere. CI's compatibility job sets
//! `MOUL_REQUIRE_LITESTREAM=1`, which turns a missing Go into a failure, and
//! the other jobs set `MOUL_SKIP_LITESTREAM=1` so the suite runs once.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use moul_ltx::{FileInfo, Ltx, Txid, file_name, parse_file_name, restore};
use serde::{Deserialize, Serialize};

/// The workspace's `target` folder.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
}

/// A fresh, empty folder for one test, under `target/litestream-compat/tmp`.
/// Folders from runs over an hour old are removed on first use; recent ones
/// stay, so a failed run can be looked at.
pub fn scratch_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    static SWEEP: OnceLock<()> = OnceLock::new();
    let tmp = target_dir().join("litestream-compat").join("tmp");
    SWEEP.get_or_init(|| {
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for e in fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < hour_ago);
            if old {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    });
    let dir = tmp
        .join(std::process::id().to_string())
        .join(format!("{name}-{}", N.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The built Go harness.
#[derive(Debug)]
pub struct Harness {
    bin: PathBuf,
}

/// The harness, built on first use, or `None` when `MOUL_SKIP_LITESTREAM` is
/// set, or when Go is not installed and `MOUL_REQUIRE_LITESTREAM` is not set.
/// Panics if the build fails.
pub fn harness() -> Option<&'static Harness> {
    static HARNESS: OnceLock<Option<Harness>> = OnceLock::new();
    HARNESS
        .get_or_init(|| {
            let set =
                |name: &str| std::env::var_os(name).is_some_and(|v| v != "0" && !v.is_empty());
            let required = set("MOUL_REQUIRE_LITESTREAM");
            if set("MOUL_SKIP_LITESTREAM") && !required {
                eprintln!(
                    "note: MOUL_SKIP_LITESTREAM is set; skipping the tests that run Litestream"
                );
                return None;
            }
            if Command::new("go").arg("version").output().is_err() {
                assert!(
                    !required,
                    "MOUL_REQUIRE_LITESTREAM is set but `go` is not installed"
                );
                eprintln!("note: Go is not installed; skipping the tests that run Litestream");
                return None;
            }
            let bin = target_dir()
                .join("litestream-compat")
                .join(format!("harness{}", std::env::consts::EXE_SUFFIX));
            let out = Command::new("go")
                .args(["build", "-o"])
                .arg(&bin)
                .arg(".")
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("run go build");
            assert!(
                out.status.success(),
                "building the Go harness failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(Harness { bin })
        })
        .as_ref()
}

/// Skips the calling test, returning from it, when Go is not available.
#[macro_export]
macro_rules! harness_or_skip {
    () => {
        match $crate::harness() {
            Some(h) => h,
            None => return,
        }
    };
}

/// A header as the harness reads and prints it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderJson {
    /// Header flags.
    pub flags: u32,
    /// Page size.
    pub page_size: u32,
    /// Database size in pages after the file.
    pub commit: u32,
    /// First TXID.
    pub min_txid: u64,
    /// Last TXID.
    pub max_txid: u64,
    /// Milliseconds since the epoch.
    pub timestamp: i64,
    /// Pre-apply checksum.
    pub pre_apply_checksum: u64,
    /// Node ID.
    pub node_id: u64,
}

/// One page, its data in hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageJson {
    /// Page number.
    pub pgno: u32,
    /// Page bytes, hex-encoded.
    pub data: String,
}

/// A whole LTX file as the harness reads and prints it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileJson {
    /// The header.
    pub header: HeaderJson,
    /// The pages, in order.
    pub pages: Vec<PageJson>,
    /// Trailer: post-apply checksum.
    pub post_apply_checksum: u64,
    /// Trailer: file checksum. Ignored by `encode`.
    pub file_checksum: u64,
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(DIGITS[(x >> 4) as usize] as char);
        s.push(DIGITS[(x & 15) as usize] as char);
    }
    s
}

/// Bytes from hex.
pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

impl FileJson {
    /// Describes a decoded file in the harness's terms.
    pub fn from_ltx(ltx: &Ltx<'_>) -> Self {
        let h = ltx.header();
        Self {
            header: HeaderJson {
                flags: h.flags,
                page_size: h.page_size,
                commit: h.commit,
                min_txid: h.min_txid.0,
                max_txid: h.max_txid.0,
                timestamp: h.timestamp,
                pre_apply_checksum: h.pre_apply_checksum.0,
                node_id: h.node_id,
            },
            pages: ltx
                .iter_pages()
                .map(|r| {
                    let (pgno, data) = r.expect("page");
                    PageJson {
                        pgno,
                        data: hex(&data),
                    }
                })
                .collect(),
            post_apply_checksum: ltx.trailer().post_apply_checksum.0,
            file_checksum: ltx.trailer().file_checksum.0,
        }
    }
}

impl Harness {
    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the harness");
        {
            let mut input = child.stdin.take().expect("stdin");
            if let Some(b) = stdin {
                input.write_all(b).expect("write to the harness");
            }
        }
        let out = child.wait_with_output().expect("wait for the harness");
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    fn path_arg(p: &Path) -> &str {
        p.to_str().expect("UTF-8 path")
    }

    /// Encodes `spec` with the Go library.
    pub fn encode(&self, spec: &FileJson) -> Result<Vec<u8>, String> {
        let dir = scratch_dir("encode");
        let out = dir.join("out.ltx");
        let json = serde_json::to_vec(spec).expect("json");
        self.run(&["encode", "-o", Self::path_arg(&out)], Some(&json))?;
        Ok(fs::read(&out).expect("read the encoded file"))
    }

    /// Decodes and verifies `bytes` with the Go library.
    pub fn dump(&self, bytes: &[u8]) -> Result<FileJson, String> {
        let dir = scratch_dir("dump");
        let path = dir.join("in.ltx");
        fs::write(&path, bytes).unwrap();
        let out = self.run(&["dump", Self::path_arg(&path)], None)?;
        Ok(serde_json::from_slice(&out).expect("dump output"))
    }

    /// Compacts `inputs` with the Go library's compactor.
    pub fn compact(&self, inputs: &[&[u8]]) -> Result<Vec<u8>, String> {
        let dir = scratch_dir("compact");
        let out = dir.join("out.ltx");
        let mut args = vec![
            "compact".to_owned(),
            "-o".into(),
            Self::path_arg(&out).into(),
        ];
        for (i, b) in inputs.iter().enumerate() {
            let p = dir.join(format!("{i}.ltx"));
            fs::write(&p, b).unwrap();
            args.push(Self::path_arg(&p).into());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args, None)?;
        Ok(fs::read(&out).unwrap())
    }

    /// `ltx.ChecksumPage` in Go.
    pub fn page_checksum(&self, pgno: u32, data: &[u8]) -> u64 {
        let out = self
            .run(&["checksum", &pgno.to_string(), &hex(data)], None)
            .expect("checksum");
        String::from_utf8(out).unwrap().trim().parse().unwrap()
    }

    /// Runs `script` against a new database at `db` that Litestream replicates
    /// into `replica`. See `main.go` for the directives.
    pub fn replicate(&self, db: &Path, replica: &Path, script: &str) -> Result<(), String> {
        self.run(
            &[
                "replicate",
                "-db",
                Self::path_arg(db),
                "-replica",
                Self::path_arg(replica),
            ],
            Some(script.as_bytes()),
        )
        .map(|_| ())
    }

    /// `litestream restore`, with a full integrity check, from the file
    /// replica at `replica`. Returns the database's bytes.
    pub fn restore(&self, replica: &Path, txid: Option<Txid>) -> Result<Vec<u8>, String> {
        let dir = scratch_dir("restore");
        let out = dir.join("restored.db");
        let txid = txid.map_or(0, |t| t.0).to_string();
        self.run(
            &[
                "restore",
                "-replica",
                Self::path_arg(replica),
                "-o",
                Self::path_arg(&out),
                "-txid",
                &txid,
            ],
            None,
        )?;
        Ok(fs::read(&out).unwrap())
    }

    /// Litestream's compactor, compacting level `level - 1` of `replica` into
    /// `level`.
    pub fn compact_replica(&self, replica: &Path, level: u8) -> Result<(), String> {
        self.run(
            &[
                "compact-replica",
                "-replica",
                Self::path_arg(replica),
                "-level",
                &level.to_string(),
            ],
            None,
        )
        .map(|_| ())
    }
}

// ---- A replica in a folder, laid out as Litestream lays it out -------------------

/// Writes `bytes`, an LTX file, into `replica` at `level`, named by its range.
pub fn put_file(replica: &Path, level: u8, bytes: &[u8]) {
    let ltx = Ltx::decode(bytes).expect("a valid LTX file");
    let h = ltx.header();
    let dir = replica.join("ltx").join(level.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(file_name(h.min_txid, h.max_txid)), bytes).unwrap();
}

/// Lists every LTX file in `replica`.
pub fn list(replica: &Path) -> Vec<FileInfo> {
    let mut out = Vec::new();
    for level in 0..=moul_ltx::SNAPSHOT_LEVEL {
        let Ok(entries) = fs::read_dir(replica.join("ltx").join(level.to_string())) else {
            continue;
        };
        for e in entries {
            let e = e.unwrap();
            let name = e.file_name();
            let Some((min, max)) = name.to_str().and_then(parse_file_name) else {
                continue;
            };
            let meta = e.metadata().unwrap();
            let created_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            out.push(FileInfo {
                level,
                min_txid: min,
                max_txid: max,
                size: meta.len(),
                created_at,
            });
        }
    }
    out.sort_by_key(|f| (f.level, f.min_txid, f.max_txid));
    out
}

/// Reads the file `info` describes.
pub fn read_file(replica: &Path, info: &FileInfo) -> Vec<u8> {
    fs::read(replica.join(info.path())).unwrap()
}

/// Restores `replica` to `target`, or to its newest TXID, with Moul's code:
/// [`restore::plan`], then [`restore::restore`].
pub fn moul_restore(replica: &Path, target: Option<Txid>) -> Result<Vec<u8>, moul_ltx::Error> {
    let plan = restore::plan(&list(replica), target)?;
    let files: Vec<Vec<u8>> = plan.iter().map(|f| read_file(replica, f)).collect();
    let decoded = files
        .iter()
        .map(|b| Ltx::decode(b))
        .collect::<Result<Vec<_>, _>>()?;
    restore::restore_to_vec(&decoded)
}
