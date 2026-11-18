// Command litestream-compat drives Litestream 0.5.13 and the Go LTX library
// for Moul's Litestream compatibility suite. The Rust tests in this folder
// build and run it; it is not meant for people.
//
//	encode   -o FILE                      LTX file from a JSON spec on stdin
//	dump     FILE                         LTX file as JSON on stdout, verified
//	compact  -o FILE IN...                Go LTX compactor over IN, in order
//	checksum PGNO HEX                     ltx.ChecksumPage of one page
//	replicate -db PATH -replica DIR       run a SQL script from stdin under Litestream
//	restore  -replica DIR -o PATH [-txid] litestream restore from a file replica
//	compact-replica -replica DIR -level N Litestream's compactor on a file replica
//
// Specs and dumps use hex for page data and integers for everything else.
package main

import (
	"bufio"
	"bytes"
	"context"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"os"
	"strconv"
	"strings"

	"github.com/benbjohnson/litestream"
	"github.com/benbjohnson/litestream/file"
	"github.com/superfly/ltx"
	_ "modernc.org/sqlite"
)

func main() {
	if len(os.Args) < 2 {
		fail(errors.New("usage: litestream-compat <command> [flags]"))
	}
	cmd, args := os.Args[1], os.Args[2:]
	var err error
	switch cmd {
	case "encode":
		err = encode(args)
	case "dump":
		err = dump(args)
	case "compact":
		err = compact(args)
	case "checksum":
		err = checksum(args)
	case "replicate":
		err = replicate(args)
	case "restore":
		err = restore(args)
	case "compact-replica":
		err = compactReplica(args)
	default:
		err = fmt.Errorf("unknown command %q", cmd)
	}
	if err != nil {
		fail(err)
	}
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "litestream-compat:", err)
	os.Exit(1)
}

type headerJSON struct {
	Flags            uint32 `json:"flags"`
	PageSize         uint32 `json:"page_size"`
	Commit           uint32 `json:"commit"`
	MinTXID          uint64 `json:"min_txid"`
	MaxTXID          uint64 `json:"max_txid"`
	Timestamp        int64  `json:"timestamp"`
	PreApplyChecksum uint64 `json:"pre_apply_checksum"`
	NodeID           uint64 `json:"node_id"`
}

type pageJSON struct {
	Pgno uint32 `json:"pgno"`
	Data string `json:"data"`
}

type fileJSON struct {
	Header            headerJSON `json:"header"`
	Pages             []pageJSON `json:"pages"`
	PostApplyChecksum uint64     `json:"post_apply_checksum"`
	FileChecksum      uint64     `json:"file_checksum"`
}

func encode(args []string) error {
	fs := flag.NewFlagSet("encode", flag.ExitOnError)
	out := fs.String("o", "", "output path")
	_ = fs.Parse(args)

	var spec fileJSON
	if err := json.NewDecoder(os.Stdin).Decode(&spec); err != nil {
		return err
	}
	h := spec.Header
	fspec := ltx.FileSpec{
		Header: ltx.Header{
			Version:          ltx.Version,
			Flags:            h.Flags,
			PageSize:         h.PageSize,
			Commit:           h.Commit,
			MinTXID:          ltx.TXID(h.MinTXID),
			MaxTXID:          ltx.TXID(h.MaxTXID),
			Timestamp:        h.Timestamp,
			PreApplyChecksum: ltx.Checksum(h.PreApplyChecksum),
			NodeID:           h.NodeID,
		},
		Trailer: ltx.Trailer{PostApplyChecksum: ltx.Checksum(spec.PostApplyChecksum)},
	}
	for _, p := range spec.Pages {
		data, err := hex.DecodeString(p.Data)
		if err != nil {
			return err
		}
		fspec.Pages = append(fspec.Pages, ltx.PageSpec{Header: ltx.PageHeader{Pgno: p.Pgno}, Data: data})
	}
	var buf bytes.Buffer
	if _, err := fspec.WriteTo(&buf); err != nil {
		return err
	}
	return os.WriteFile(*out, buf.Bytes(), 0o644)
}

func dump(args []string) error {
	if len(args) != 1 {
		return errors.New("dump takes one file")
	}
	f, err := os.Open(args[0])
	if err != nil {
		return err
	}
	defer f.Close()
	var spec ltx.FileSpec
	if _, err := spec.ReadFrom(f); err != nil {
		return err
	}
	h := spec.Header
	out := fileJSON{
		Header: headerJSON{
			Flags: h.Flags, PageSize: h.PageSize, Commit: h.Commit,
			MinTXID: uint64(h.MinTXID), MaxTXID: uint64(h.MaxTXID), Timestamp: h.Timestamp,
			PreApplyChecksum: uint64(h.PreApplyChecksum), NodeID: h.NodeID,
		},
		Pages:             []pageJSON{},
		PostApplyChecksum: uint64(spec.Trailer.PostApplyChecksum),
		FileChecksum:      uint64(spec.Trailer.FileChecksum),
	}
	for _, p := range spec.Pages {
		out.Pages = append(out.Pages, pageJSON{Pgno: p.Header.Pgno, Data: hex.EncodeToString(p.Data)})
	}
	return json.NewEncoder(os.Stdout).Encode(out)
}

func compact(args []string) error {
	fs := flag.NewFlagSet("compact", flag.ExitOnError)
	out := fs.String("o", "", "output path")
	_ = fs.Parse(args)

	var rdrs []io.Reader
	for _, path := range fs.Args() {
		b, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		rdrs = append(rdrs, bytes.NewReader(b))
	}
	var buf bytes.Buffer
	c, err := ltx.NewCompactor(&buf, rdrs)
	if err != nil {
		return err
	}
	c.HeaderFlags = ltx.HeaderFlagNoChecksum
	if err := c.Compact(context.Background()); err != nil {
		return err
	}
	return os.WriteFile(*out, buf.Bytes(), 0o644)
}

func checksum(args []string) error {
	if len(args) != 2 {
		return errors.New("checksum takes a page number and hex data")
	}
	pgno, err := strconv.ParseUint(args[0], 10, 32)
	if err != nil {
		return err
	}
	data, err := hex.DecodeString(args[1])
	if err != nil {
		return err
	}
	fmt.Println(uint64(ltx.ChecksumPage(uint32(pgno), data)))
	return nil
}

func quietLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

// replicate runs a SQL script against a database that Litestream replicates
// to a file replica. Lines starting with "-- " are directives, run in order:
//
//	-- sync          Litestream syncs the WAL into a level-0 file and uploads it
//	-- compact N     compact level N-1 into level N
//	-- snapshot      write a level-9 snapshot
//	-- checkpoint    PRAGMA wal_checkpoint(TRUNCATE)
//
// Anything else is SQL, run as one batch up to the next directive.
func replicate(args []string) error {
	fs := flag.NewFlagSet("replicate", flag.ExitOnError)
	dbPath := fs.String("db", "", "database path")
	replicaDir := fs.String("replica", "", "replica directory")
	_ = fs.Parse(args)
	ctx := context.Background()

	conn, err := sql.Open("sqlite", *dbPath)
	if err != nil {
		return err
	}
	defer conn.Close()
	conn.SetMaxOpenConns(1)

	db := litestream.NewDB(*dbPath)
	db.Logger = quietLogger()
	db.MonitorInterval = 0 // syncs happen when the script says so
	db.CheckpointInterval = 0
	db.Replica = litestream.NewReplicaWithClient(db, file.NewReplicaClient(*replicaDir))
	db.Replica.MonitorEnabled = false
	opened := false
	defer func() {
		if opened {
			_ = db.Close(ctx)
		}
	}()

	sync := func() error {
		if !opened {
			if err := db.Open(); err != nil {
				return fmt.Errorf("open litestream db: %w", err)
			}
			opened = true
		}
		if err := db.Sync(ctx); err != nil {
			return fmt.Errorf("sync: %w", err)
		}
		if err := db.Replica.Sync(ctx); err != nil {
			return fmt.Errorf("replica sync: %w", err)
		}
		return nil
	}

	var batch strings.Builder
	flush := func() error {
		s := strings.TrimSpace(batch.String())
		batch.Reset()
		if s == "" {
			return nil
		}
		if _, err := conn.ExecContext(ctx, s); err != nil {
			return fmt.Errorf("exec %q: %w", s, err)
		}
		return nil
	}

	sc := bufio.NewScanner(os.Stdin)
	sc.Buffer(make([]byte, 1<<20), 1<<26)
	for sc.Scan() {
		line := sc.Text()
		if !strings.HasPrefix(line, "-- ") {
			batch.WriteString(line)
			batch.WriteByte('\n')
			continue
		}
		if err := flush(); err != nil {
			return err
		}
		fields := strings.Fields(strings.TrimPrefix(line, "-- "))
		switch {
		case len(fields) == 1 && fields[0] == "sync":
			err = sync()
		case len(fields) == 2 && fields[0] == "compact":
			var level int
			if level, err = strconv.Atoi(fields[1]); err == nil {
				if err = sync(); err == nil {
					_, err = db.Compact(ctx, level)
				}
			}
		case len(fields) == 1 && fields[0] == "snapshot":
			if err = sync(); err == nil {
				_, err = db.Snapshot(ctx)
			}
		case len(fields) == 1 && fields[0] == "checkpoint":
			_, err = conn.ExecContext(ctx, "PRAGMA wal_checkpoint(TRUNCATE)")
		default:
			err = fmt.Errorf("unknown directive %q", line)
		}
		if err != nil {
			return err
		}
	}
	if err := sc.Err(); err != nil {
		return err
	}
	return flush()
}

func restore(args []string) error {
	fs := flag.NewFlagSet("restore", flag.ExitOnError)
	replicaDir := fs.String("replica", "", "replica directory")
	out := fs.String("o", "", "output path")
	txid := fs.Uint64("txid", 0, "restore to this TXID")
	_ = fs.Parse(args)

	r := litestream.NewReplicaWithClient(nil, file.NewReplicaClient(*replicaDir))
	opt := litestream.NewRestoreOptions()
	opt.OutputPath = *out
	opt.TXID = ltx.TXID(*txid)
	opt.IntegrityCheck = litestream.IntegrityCheckFull
	return r.Restore(context.Background(), opt)
}

func compactReplica(args []string) error {
	fs := flag.NewFlagSet("compact-replica", flag.ExitOnError)
	replicaDir := fs.String("replica", "", "replica directory")
	level := fs.Int("level", 1, "destination level")
	_ = fs.Parse(args)

	c := litestream.NewCompactor(file.NewReplicaClient(*replicaDir), quietLogger())
	_, err := c.Compact(context.Background(), *level)
	return err
}
