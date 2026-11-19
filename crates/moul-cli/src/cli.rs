//! The commands.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use moul_ltx::{SNAPSHOT_LEVEL, Txid};
use moul_store::Store;
use moul_sync::layout::{HEAD_KEY, Head, WRITER_KEY, WriterRecord, list_files};
use moul_sync::{Options, Prefixed, Replica, Role, StoreLocal, Sync, block_on, rfc3339};
use moul_vfs::{Capture, DiskStorage, Volume};

const USAGE: &str = "\
moul: the Moul command-line tool

Usage:
  moul serve [<folder>] [--listen <addr>] [--public-url <url>] [--replica <replica>]
             [--from <file>] [--admin-key <key>] [--cors <origins>] [--read-only]
      Serve the database in <folder> (default ./moul-data) over HTTP: the
      API, live events, sign-in with OAuth providers, mail, webhooks, jobs
      and MCP at /api/mcp. Listens on 127.0.0.1:8090 unless --listen says
      otherwise (a port, or an address and port such as 0.0.0.0:8090).
      --replica pushes every commit to a replica, as Litestream does.
      --from fills a new folder from a database file: a Moul export or a
      Go engine database. --read-only serves a folder `moul pull` keeps.
      The admin key is made on first start and printed once, unless
      MOUL_ADMIN_KEY or --admin-key gives one.
  moul mcp <folder> [--read-only]
      Serve the database in <folder> to an AI agent as MCP tools over
      stdin and stdout. With --read-only (a folder `moul pull` keeps) the
      agent can read but not change it.
  moul hub [<folder>] [--listen <addr>] [--public-url <url>] [--store <store>]
           [--signup open|invite|closed] [--max-databases <n>] [--max-bytes <n>]
      Run a sync hub for builders' devices: sign-in by email code, a
      replica for each database in <folder>/replicas or in --store (a folder
      or an s3:// URL), push events and compaction. Listens on
      127.0.0.1:8091 unless --listen says otherwise.
  moul login <hub> [--email <address>] [--label <name>]
      Sign this computer in to a hub with an email code, and keep the token
      in ~/.config/moul/hubs.json for pull, restore, info and serve.
  moul logout <hub>
      Revoke this computer's token at the hub and forget it.
  moul restore <replica> -o <file> [--txid <n>] [--writer-only] [--force]
  moul restore <replica> -o <folder>/ [--force]
      Rebuild the database a replica holds into a plain SQLite file, as
      `litestream restore` does. --txid restores an earlier state.
      --writer-only restores the writer-only store instead (root users,
      password hashes, keys, settings, the outbox), which a server pushes
      beside the shared store while it writes. Given a folder (one that
      exists, or a path ending in /), restores both as main.db and
      writer.db, ready for `moul serve <folder>`.
  moul pull <replica> <folder> [--watch] [--every <seconds>]
      Keep a local copy of a replica in <folder>, following its writer.
      <folder>/main.db is a SQLite file other tools can read while moul is
      not running. --watch keeps pulling until stopped, every 3 seconds or
      every --every seconds (0.2 to 86400).
  moul info <replica>
      Show who writes a replica, how far it reaches and what it holds.
  moul schema <database> [-o <file>]
      Write the app definition (collections, fields with their ids, rules)
      as a schema file, for git, review and agents.
  moul typegen <schema file | database> [-o <file>]
      Write TypeScript types for the records of every collection.
  moul check [<schema file | database>] [--json] [--strict]
      Check an app definition before it is applied: the file reads, every
      collection applies to an empty app, relations have targets and every
      rule compiles against its collection. Rules that let anyone write
      are warnings. Checks ./moul.schema.json when no file is given.
      --json prints one JSON report. Exits 0 when there is no error (and,
      with --strict, no warning), 1 when there is, and 2 when the file
      cannot be read.

A database is a SQLite file (main.db, a Moul export or a Go engine
database) or a folder holding main.db, such as one `moul pull` keeps.

A replica is a folder (a path or file:///path), a database on a Moul hub:
  moul+https://<hub>/<database>
with the token `moul login` saved (or MOUL_HUB_TOKEN), or an S3-compatible
bucket:
  s3://<bucket>/<prefix>?endpoint=<url>&region=<region>
with credentials from AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and
AWS_SESSION_TOKEN. The endpoint and region may also come from
AWS_ENDPOINT_URL and AWS_REGION. For Cloudflare R2 use
  s3://<bucket>/<prefix>?endpoint=https://<account>.r2.cloudflarestorage.com
";

pub(crate) type Result<T> = std::result::Result<T, String>;

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
        Some("schema") => schema(&args[1..]),
        Some("typegen") => typegen(&args[1..]),
        Some("check") => return check(&args[1..]),
        Some("serve") => crate::serve::serve(&args[1..]),
        Some("mcp") => crate::serve::mcp(&args[1..]),
        Some("hub") => crate::hub::hub(&args[1..]),
        Some("login") => crate::hub::login(&args[1..]),
        Some("logout") => crate::hub::logout(&args[1..]),
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
pub(crate) struct Args {
    pub(crate) positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    pub(crate) fn parse(args: &[String], valued: &[&str], bare: &[&str]) -> Result<Args> {
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

    pub(crate) fn value(&self, names: &[&str]) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| names.contains(&k.as_str()))
            .and_then(|(_, v)| v.as_deref())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }

    pub(crate) fn positional(&self, n: usize, what: &str) -> Result<&str> {
        self.positional
            .get(n)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {what}; see `moul help`"))
    }

    pub(crate) fn arity(&self, n: usize) -> Result<()> {
        match self.positional.get(n) {
            Some(extra) => Err(format!("unexpected argument {extra:?}")),
            None => Ok(()),
        }
    }
}

/// Opens the replica `spec` names: a folder, an `s3://` bucket, or a
/// `moul+https://` hub database.
fn open_replica(spec: &str) -> Result<Box<dyn Replica>> {
    moul_server::replica::open(spec, None, None)
}

fn restore(args: &[String]) -> Result<()> {
    let a = Args::parse(
        args,
        &["-o", "--output", "--txid"],
        &["--force", "-f", "--writer-only"],
    )?;
    let spec = a.positional(0, "the replica")?;
    a.arity(1)?;
    let out_arg = a
        .value(&["-o", "--output"])
        .ok_or("missing -o <file>: where to write the database")?;
    let out = PathBuf::from(out_arg);
    let target = match a.value(&["--txid"]) {
        Some(t) => Some(Txid(parse_txid(t)?)),
        None => None,
    };
    let force = a.has("--force") || a.has("-f");
    let writer_only = a.has("--writer-only");
    if out_arg.ends_with(std::path::is_separator) || out.is_dir() {
        if target.is_some() {
            return Err(
                "--txid is a moment in one store; give -o <file> to restore one store at a TXID"
                    .into(),
            );
        }
        if writer_only {
            return Err("--writer-only restores one store; give -o <file>".into());
        }
        let replica = open_replica(spec)?;
        return restore_folder(replica.as_ref(), &out, force);
    }
    if out.exists() && !force {
        return Err(format!(
            "{} exists; pass --force to replace it",
            out.display()
        ));
    }
    let replica = open_replica(spec)?;
    let prefixed;
    let source: &dyn Replica = if writer_only {
        prefixed = Prefixed::writer_only(replica.as_ref());
        if block_on(list_files(&prefixed))
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Err(no_writer_only(replica.as_ref()));
        }
        &prefixed
    } else {
        replica.as_ref()
    };
    let (header, db) =
        block_on(moul_sync::restore::restore_db(source, target)).map_err(|e| e.to_string())?;
    let tmp = stage(&out, &db)?;
    fs::rename(&tmp, &out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    println!(
        "restored {}TXID {} ({} pages of {} bytes) to {}",
        if writer_only {
            "the writer-only store at "
        } else {
            ""
        },
        header.max_txid.0,
        header.commit,
        header.page_size,
        out.display()
    );
    Ok(())
}

fn no_writer_only(replica: &dyn Replica) -> String {
    format!(
        "{} holds no writer-only store; a server pushes one while it writes (moul serve --replica)",
        replica.describe()
    )
}

/// Writes `db` beside `out` and checks it, without replacing `out` yet.
/// Returns the file written, for the caller to rename into place.
fn stage(out: &Path, db: &[u8]) -> Result<PathBuf> {
    let tmp = out.with_extension("moul-restore.tmp");
    {
        let mut f =
            File::create(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.write_all(db)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    if let Err(e) = check_integrity(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

/// Restores both stores into `dir` as `main.db` and `writer.db`: a folder
/// `moul serve` opens with its root users, keys and settings. Only into a
/// folder that holds nothing else, because a server's folder holds a commit
/// log that would replay over the restored files.
fn restore_folder(replica: &dyn Replica, dir: &Path, force: bool) -> Result<()> {
    const FILES: [&str; 2] = ["main.db", "writer.db"];
    if dir.exists() {
        let entries =
            fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".moul-restore.tmp") {
                continue; // left by a restore that stopped; written afresh
            }
            if !FILES.contains(&name.as_str()) {
                return Err(format!(
                    "{} holds other files ({name}); restore into a new folder",
                    dir.display()
                ));
            }
            if !force {
                return Err(format!(
                    "{} exists; pass --force to replace it",
                    entry.path().display()
                ));
            }
        }
    }
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let restore = |r: &dyn Replica| {
        block_on(moul_sync::restore::restore_db(r, None)).map_err(|e| e.to_string())
    };
    let shared = restore(replica)?;
    let prefixed = Prefixed::writer_only(replica);
    let writer_only = if block_on(list_files(&prefixed))
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        None
    } else {
        Some(restore(&prefixed)?)
    };
    // Both are checked before either lands.
    let main_path = dir.join(FILES[0]);
    let writer_path = dir.join(FILES[1]);
    let main_tmp = stage(&main_path, &shared.1)?;
    let writer_tmp = match &writer_only {
        Some((_, db)) => match stage(&writer_path, db) {
            Ok(t) => Some(t),
            Err(e) => {
                let _ = fs::remove_file(&main_tmp);
                return Err(e);
            }
        },
        None => None,
    };
    let write = |e: std::io::Error| format!("cannot write into {}: {e}", dir.display());
    fs::rename(&main_tmp, &main_path).map_err(write)?;
    match writer_tmp {
        Some(t) => fs::rename(&t, &writer_path).map_err(write)?,
        // A writer.db left from before belongs to another database.
        None if writer_path.exists() => fs::remove_file(&writer_path).map_err(write)?,
        None => {}
    }
    let h = shared.0;
    println!(
        "restored TXID {} ({} pages of {} bytes) to {}",
        h.max_txid.0,
        h.commit,
        h.page_size,
        main_path.display()
    );
    match writer_only {
        Some((h, _)) => println!(
            "restored the writer-only store at TXID {} ({} pages of {} bytes) to {}",
            h.max_txid.0,
            h.commit,
            h.page_size,
            writer_path.display()
        ),
        None => println!("note: {}", no_writer_only(replica)),
    }
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

/// Checks the SQLite file at `path`. It opens the file as immutable, so a file
/// in WAL mode gets no `-wal` or `-shm` file next to it.
fn check_integrity(path: &Path) -> Result<()> {
    let conn = rusqlite::Connection::open_with_flags(
        immutable_uri(path),
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

/// The SQLite URI that opens `path` as an immutable file. Every byte but
/// letters, digits and `-._~` is percent-encoded, slashes too. SQLite decodes
/// them back, so no `?`, `#`, `%` or leading `//` in the path reads as URI
/// syntax, and any path, relative or absolute, names the same file.
fn immutable_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for &b in path.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    uri
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
    let every = match a.value(&["--every"]) {
        Some(s) => parse_every(s)?,
        None => Duration::from_secs(3),
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
        std::thread::sleep(every);
    }
}

/// The shortest pause `moul pull --every` takes, in seconds.
const EVERY_MIN: f64 = 0.2;

/// The longest pause `moul pull --every` takes, in seconds: a day.
const EVERY_MAX: f64 = 86_400.0;

/// Reads the seconds `--every` gives. Infinite, NaN, negative and huge values
/// are refused here, so the pull loop never sleeps on one.
fn parse_every(s: &str) -> Result<Duration> {
    s.parse::<f64>()
        .ok()
        .filter(|v| (EVERY_MIN..=EVERY_MAX).contains(v))
        .and_then(|v| Duration::try_from_secs_f64(v).ok())
        .ok_or_else(|| format!("--every {s:?}: give seconds, from {EVERY_MIN} to {EVERY_MAX}"))
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
        let writer_only = Prefixed::writer_only(replica.as_ref());
        let held = list_files(&writer_only).await.map_err(err)?;
        let writer_only = match moul_ltx::restore::plan(&held, None) {
            _ if held.is_empty() => "writer-only  none".to_owned(),
            Ok(plan) => format!(
                "writer-only  TXID {}, {} files, {} bytes",
                plan.last().map_or(0, |f| f.max_txid.0),
                held.len(),
                held.iter().map(|f| f.size).sum::<u64>()
            ),
            Err(e) => format!("writer-only  restores nothing: {e}"),
        };
        let files = list_files(replica.as_ref()).await.map_err(err)?;
        if files.is_empty() {
            println!("files     none");
            println!("{writer_only}");
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
        println!("{writer_only}");
        Ok(())
    })
}

/// Reads the collections of a database file or folder, without changing it.
fn read_collections(spec: &str) -> Result<Vec<moul_store::schema::Collection>> {
    let mut path = PathBuf::from(spec);
    if path.is_dir() {
        path = path.join("main.db");
    }
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let has_moul: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '_moul'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_moul {
        return Err(format!(
            "{} is not a Moul database (no _moul table)",
            path.display()
        ));
    }
    moul_store::catalog::load_all(&conn).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_output(out: Option<&str>, text: &str) -> Result<()> {
    match out {
        Some(file) => fs::write(file, text).map_err(|e| format!("{file}: {e}")),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

fn schema(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &["-o", "--output"], &[])?;
    let db = a.positional(0, "a database")?;
    a.arity(1)?;
    let collections = read_collections(db)?;
    let file = moul_core::definition::schema_file(&collections);
    let mut text = serde_json::to_string_pretty(&file).expect("json serialises");
    text.push('\n');
    write_output(a.value(&["-o", "--output"]), &text)
}

fn typegen(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &["-o", "--output"], &[])?;
    let input = a.positional(0, "a schema file or a database")?;
    a.arity(1)?;
    let collections = if input.ends_with(".json") {
        let text = fs::read_to_string(input).map_err(|e| format!("{input}: {e}"))?;
        let file: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{input}: {e}"))?;
        moul_core::definition::read_schema_file(&file).map_err(|e| format!("{input}: {e}"))?
    } else {
        read_collections(input)?
    };
    write_output(
        a.value(&["-o", "--output"]),
        &moul_core::definition::typescript(&collections),
    )
}

/// The schema file `moul check` reads when given none.
const DEFAULT_SCHEMA: &str = "moul.schema.json";

/// The first bytes of every SQLite database file.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// `moul check`. Exits 0 when the definition is sound, 1 when it has
/// errors (or warnings, with `--strict`), and 2 when it cannot be checked:
/// a bad argument or a file that cannot be read.
fn check(args: &[String]) -> ExitCode {
    let run = || -> Result<bool> {
        let a = Args::parse(args, &[], &["--json", "--strict"])?;
        a.arity(1)?;
        let shown = match a.positional.first() {
            Some(p) => p.clone(),
            None if Path::new(DEFAULT_SCHEMA).is_file() => DEFAULT_SCHEMA.to_owned(),
            None => {
                return Err(format!(
                    "no {DEFAULT_SCHEMA} here; give a schema file or a database to check"
                ));
            }
        };
        let input = read_check_input(&shown)?;
        let strict = a.has("--strict");
        let report = crate::check::check(input, &shown)?;
        if a.has("--json") {
            print!("{}", report.to_json(strict));
        } else {
            print!("{}", report.to_text(strict));
        }
        Ok(report.passes(strict))
    };
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("moul: {e}");
            ExitCode::from(2)
        }
    }
}

/// A database (a SQLite file, or a folder holding main.db) or the bytes of
/// a schema file. A database is opened read-only.
fn read_check_input(spec: &str) -> Result<crate::check::Input> {
    let path = Path::new(spec);
    let unreadable = |e: std::io::Error| format!("cannot read {spec}: {e}");
    if path.is_dir() {
        return Ok(crate::check::Input::Database(read_collections(spec)?));
    }
    let mut head = Vec::with_capacity(SQLITE_HEADER.len());
    File::open(path)
        .and_then(|f| f.take(SQLITE_HEADER.len() as u64).read_to_end(&mut head))
        .map_err(unreadable)?;
    if head == SQLITE_HEADER {
        return Ok(crate::check::Input::Database(read_collections(spec)?));
    }
    Ok(crate::check::Input::Schema(
        fs::read(path).map_err(unreadable)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty folder for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("moul-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a small, valid database in WAL mode at `path`.
    fn valid_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; CREATE TABLE t (x); INSERT INTO t VALUES (1);",
        )
        .unwrap();
    }

    /// Names SQLite would read as URI syntax if they were not escaped.
    fn awkward_names() -> Vec<&'static str> {
        let mut names = vec![
            "a b.db",
            "c#d.db",
            "e%41.db",
            "f%.db",
            "g%2.db",
            "~h-i_j.db",
        ];
        if cfg!(unix) {
            names.push("k?immutable=0.db");
        }
        names
    }

    #[test]
    fn immutable_uri_escapes_the_path() {
        assert_eq!(
            immutable_uri(Path::new("/d b/c#d?e%41~-_.db")),
            "file:%2Fd%20b%2Fc%23d%3Fe%2541~-_.db?immutable=1"
        );
        assert_eq!(immutable_uri(Path::new("//x")), "file:%2F%2Fx?immutable=1");
    }

    #[test]
    fn check_integrity_reads_the_file_it_is_given() {
        let dir = scratch("check-wrong-file");
        // A valid database sits where an unescaped URI would stop or decode
        // the path; the file to check next to it is garbage.
        let mut pairs = vec![("db", "db#2.tmp"), ("dbA.tmp", "db%41.tmp")];
        if cfg!(unix) {
            pairs.push(("dc", "dc?x.tmp"));
        }
        for (decoy, name) in pairs {
            valid_db(&dir.join(decoy));
            fs::write(dir.join(name), vec![0xab; 4096]).unwrap();
            assert!(check_integrity(&dir.join(name)).is_err(), "{name}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_integrity_opens_awkward_names() {
        let dir = scratch("check-awkward");
        valid_db(&dir.join("good.db"));
        let good = fs::read(dir.join("good.db")).unwrap();
        for name in awkward_names() {
            fs::write(dir.join(name), &good).unwrap();
            check_integrity(&dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        // Immutable: no -wal or -shm file appears next to any of them.
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want: Vec<String> = awkward_names().into_iter().map(String::from).collect();
        want.push("good.db".into());
        want.sort();
        assert_eq!(left, want);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_every_takes_seconds_from_a_fifth_to_a_day() {
        assert_eq!(parse_every("0.2").unwrap(), Duration::from_millis(200));
        assert_eq!(parse_every("3").unwrap(), Duration::from_secs(3));
        assert_eq!(parse_every("86400").unwrap(), Duration::from_secs(86_400));
        for bad in [
            "inf", "infinity", "-inf", "NaN", "1e20", "86401", "-1", "0", "0.1", "", "soon",
        ] {
            let e = parse_every(bad).unwrap_err();
            assert!(e.contains("from 0.2 to 86400"), "{bad}: {e}");
        }
    }
}
