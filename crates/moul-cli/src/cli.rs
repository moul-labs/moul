//! The commands.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use moul_ltx::{SNAPSHOT_LEVEL, Txid};
use moul_store::Store;
use moul_sync::layout::{HEAD_KEY, Head, WRITER_KEY, WriterRecord, list_files};
use moul_sync::{
    DirReplica, Options, Replica, Role, S3Config, S3Replica, StoreLocal, Sync, block_on, rfc3339,
};
use moul_vfs::{Capture, DiskStorage, Volume};

const USAGE: &str = "\
moul: the Moul command-line tool

Usage:
  moul restore <replica> -o <file> [--txid <n>] [--force]
      Rebuild the database a replica holds into a plain SQLite file, as
      `litestream restore` does. --txid restores an earlier state.
  moul pull <replica> <folder> [--watch] [--every <seconds>]
      Keep a local copy of a replica in <folder>, following its writer.
      <folder>/main.db is a SQLite file other tools can read while moul is
      not running. --watch keeps pulling until stopped.
  moul info <replica>
      Show who writes a replica, how far it reaches and what it holds.

A replica is a folder (a path or file:///path) or an S3-compatible bucket:
  s3://<bucket>/<prefix>?endpoint=<url>&region=<region>
with credentials from AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and
AWS_SESSION_TOKEN. The endpoint and region may also come from
AWS_ENDPOINT_URL and AWS_REGION. For Cloudflare R2 use
  s3://<bucket>/<prefix>?endpoint=https://<account>.r2.cloudflarestorage.com
";

type Result<T> = std::result::Result<T, String>;

pub(crate) fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("version" | "-V" | "--version") => {
            println!("moul {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("restore") => restore(&args[1..]),
        Some("pull") => pull(&args[1..]),
        Some("info") => info(&args[1..]),
        Some(other) => Err(format!("unknown command {other:?}; see `moul help`")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("moul: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Positional arguments and flags, with values for the flags in `valued`.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(args: &[String], valued: &[&str], bare: &[&str]) -> Result<Args> {
        let mut out = Args {
            positional: Vec::new(),
            flags: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if let Some((name, value)) = a.split_once('=').filter(|_| a.starts_with('-')) {
                if !valued.contains(&name) {
                    return Err(format!("unknown option {name}"));
                }
                out.flags.push((name.to_owned(), Some(value.to_owned())));
            } else if valued.contains(&a.as_str()) {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                out.flags.push((a.clone(), Some(v.clone())));
            } else if bare.contains(&a.as_str()) {
                out.flags.push((a.clone(), None));
            } else if a.starts_with('-') && a.len() > 1 {
                return Err(format!("unknown option {a}"));
            } else {
                out.positional.push(a.clone());
            }
        }
        Ok(out)
    }

    fn value(&self, names: &[&str]) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| names.contains(&k.as_str()))
            .and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }

    fn positional(&self, n: usize, what: &str) -> Result<&str> {
        self.positional
            .get(n)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {what}; see `moul help`"))
    }

    fn arity(&self, n: usize) -> Result<()> {
        match self.positional.get(n) {
            Some(extra) => Err(format!("unexpected argument {extra:?}")),
            None => Ok(()),
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Opens the replica `spec` names.
fn open_replica(spec: &str) -> Result<Box<dyn Replica>> {
    if spec.starts_with("s3://") {
        let key = env("AWS_ACCESS_KEY_ID")
            .ok_or("AWS_ACCESS_KEY_ID is not set; an s3:// replica needs credentials")?;
        let secret = env("AWS_SECRET_ACCESS_KEY")
            .ok_or("AWS_SECRET_ACCESS_KEY is not set; an s3:// replica needs credentials")?;
        let mut url = spec.to_owned();
        let mut add = |k: &str, v: Option<String>| {
            if let Some(v) = v
                && !url.contains(&format!("{k}="))
            {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&format!("{k}={v}"));
            }
        };
        add(
            "endpoint",
            env("AWS_ENDPOINT_URL_S3").or_else(|| env("AWS_ENDPOINT_URL")),
        );
        add(
            "region",
            env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")),
        );
        let mut config = S3Config::from_url(&url, &key, &secret).map_err(|e| e.to_string())?;
        config.session_token = env("AWS_SESSION_TOKEN");
        return Ok(Box::new(
            S3Replica::with_default_client(config).map_err(|e| e.to_string())?,
        ));
    }
    let path = spec.strip_prefix("file://").unwrap_or(spec);
    if path.contains("://") {
        return Err(format!("{spec:?} is not a folder or an s3:// URL"));
    }
    Ok(Box::new(DirReplica::new(path)))
}

fn restore(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &["-o", "--output", "--txid"], &["--force", "-f"])?;
    let spec = a.positional(0, "the replica")?;
    a.arity(1)?;
    let out = PathBuf::from(
        a.value(&["-o", "--output"])
            .ok_or("missing -o <file>: where to write the database")?,
    );
    let target = match a.value(&["--txid"]) {
        Some(t) => Some(Txid(parse_txid(t)?)),
        None => None,
    };
    if out.exists() && !(a.has("--force") || a.has("-f")) {
        return Err(format!(
            "{} exists; pass --force to replace it",
            out.display()
        ));
    }
    let replica = open_replica(spec)?;
    let (header, db) = block_on(moul_sync::restore::restore_db(replica.as_ref(), target))
        .map_err(|e| e.to_string())?;
    let tmp = out.with_extension("moul-restore.tmp");
    {
        let mut f =
            File::create(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.write_all(&db)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    if let Err(e) = check_integrity(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, &out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    println!(
        "restored TXID {} ({} pages of {} bytes) to {}",
        header.max_txid.0,
        header.commit,
        header.page_size,
        out.display()
    );
    Ok(())
}

fn parse_txid(s: &str) -> Result<u64> {
    let t = if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else if s.len() == 16 {
        u64::from_str_radix(s, 16)
    } else {
        s.parse()
    };
    t.ok()
        .filter(|&t| t > 0)
        .ok_or_else(|| format!("{s:?} is not a TXID"))
}

fn check_integrity(path: &Path) -> Result<()> {
    let uri = format!("file:{}?immutable=1", path.display());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("the restored file does not open: {e}"))?;
    let ok: String = conn
        .query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))
        .map_err(|e| format!("the restored file does not pass its integrity check: {e}"))?;
    if ok != "ok" {
        return Err(format!(
            "the restored file does not pass its integrity check: {ok}"
        ));
    }
    Ok(())
}

/// A pulled folder: the volume, its capture, and a lock so that one process
/// uses it at a time.
struct Pulled {
    store: Store,
    capture: Capture,
    conflicts: PathBuf,
    _lock: File,
}

fn open_pulled(dir: &Path) -> Result<Pulled> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let lock_path = dir.join("moul.lock");
    let lock = File::create(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    lock.try_lock()
        .map_err(|_| format!("another moul process is using {}", dir.display()))?;
    let storage = DiskStorage::new(dir).map_err(|e| e.to_string())?;
    let ltx = DiskStorage::new(dir.join("ltx")).map_err(|e| e.to_string())?;
    let capture = Capture::open(ltx, "main.db").map_err(|e| e.to_string())?;
    let volume = Volume::open_observed(storage, capture.clone()).map_err(|e| e.to_string())?;
    let store = Store::open_follower(volume).map_err(|e| e.to_string())?;
    Ok(Pulled {
        store,
        capture,
        conflicts: dir.join("conflicts"),
        _lock: lock,
    })
}

fn pull(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &["--every"], &["--watch", "-w"])?;
    let spec = a.positional(0, "the replica")?;
    let dir = PathBuf::from(a.positional(1, "the folder to pull into")?);
    a.arity(2)?;
    let watch = a.has("--watch") || a.has("-w");
    let every: f64 = match a.value(&["--every"]) {
        Some(s) => s
            .parse()
            .ok()
            .filter(|s: &f64| *s >= 0.2)
            .ok_or_else(|| format!("--every {s:?}: give seconds, at least 0.2"))?,
        None => 3.0,
    };
    let replica = open_replica(spec)?;
    let mut pulled = open_pulled(&dir)?;
    let mut sync = Sync::new(
        replica,
        Options {
            label: "moul pull".into(),
            poll_every: 0,
            follow_only: true,
            ..Options::default()
        },
    );
    let mut last = None;
    loop {
        let mut local = StoreLocal {
            store: &mut pulled.store,
            capture: pulled.capture.clone(),
            conflicts: Some(pulled.conflicts.clone()),
        };
        let result = block_on(sync.sync(&mut local));
        let status = sync.status(&pulled.capture);
        match result {
            Ok(()) => {
                if status.role == Role::Writer {
                    return Err(format!(
                        "{} was last used to write this replica; pull into another folder",
                        dir.display()
                    ));
                }
                if last != Some(status.position) {
                    let writer = status
                        .writer
                        .as_ref()
                        .map(|w| {
                            let who = if w.label.is_empty() {
                                &w.device
                            } else {
                                &w.label
                            };
                            format!(", written by {who}")
                        })
                        .unwrap_or_default();
                    println!(
                        "{} at TXID {}{writer}",
                        dir.join("main.db").display(),
                        status.position
                    );
                    for c in &status.conflicts {
                        println!("kept the folder's earlier database as {c}");
                    }
                    last = Some(status.position);
                }
            }
            Err(e) if watch && e.is_transient() => eprintln!("moul: {e}; retrying"),
            Err(e) => return Err(e.to_string()),
        }
        if !watch {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs_f64(every));
    }
}

fn info(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &[], &[])?;
    let spec = a.positional(0, "the replica")?;
    a.arity(1)?;
    let replica = open_replica(spec)?;
    block_on(async {
        let err = |e: moul_sync::Error| e.to_string();
        println!("replica   {}", replica.describe());
        let json = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match replica.get(WRITER_KEY).await.map_err(err)? {
            Some(o) => match serde_json::from_slice::<WriterRecord>(&o.bytes) {
                Ok(w) => println!(
                    "writer    {}{} (generation {}, since TXID {}, {})",
                    w.device,
                    if w.label.is_empty() {
                        String::new()
                    } else {
                        format!(" \"{}\"", w.label)
                    },
                    w.generation,
                    w.since,
                    rfc3339(w.at)
                ),
                Err(_) => println!("writer    unreadable: {}", json(&o.bytes)),
            },
            None => println!("writer    none recorded (written by Litestream, or empty)"),
        }
        if let Some(o) = replica.get(HEAD_KEY).await.map_err(err)?
            && let Ok(h) = serde_json::from_slice::<Head>(&o.bytes)
        {
            println!("head      TXID {} at {}", h.txid, rfc3339(h.at));
        }
        let files = list_files(replica.as_ref()).await.map_err(err)?;
        if files.is_empty() {
            println!("files     none");
            return Ok(());
        }
        for level in 0..=SNAPSHOT_LEVEL {
            let at: Vec<_> = files.iter().filter(|f| f.level == level).collect();
            if at.is_empty() {
                continue;
            }
            let bytes: u64 = at.iter().map(|f| f.size).sum();
            let lo = at.iter().map(|f| f.min_txid.0).min().unwrap_or(0);
            let hi = at.iter().map(|f| f.max_txid.0).max().unwrap_or(0);
            let name = if level == SNAPSHOT_LEVEL {
                "snapshots".to_owned()
            } else {
                format!("level {level}")
            };
            println!(
                "{name:<10}{} files, {} bytes, TXIDs {lo}-{hi}",
                at.len(),
                bytes
            );
        }
        match moul_ltx::restore::plan(&files, None) {
            Ok(plan) => {
                let newest = plan.last().map_or(0, |f| f.max_txid.0);
                let oldest = files
                    .iter()
                    .filter(|f| f.level == SNAPSHOT_LEVEL || f.min_txid.0 == 1)
                    .map(|f| f.max_txid.0)
                    .min()
                    .unwrap_or(newest);
                println!(
                    "restores  TXIDs {oldest} to {newest}, from {} files",
                    plan.len()
                );
            }
            Err(e) => println!("restores  nothing: {e}"),
        }
        Ok(())
    })
}
