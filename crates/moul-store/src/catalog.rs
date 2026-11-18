//! Collections as stored: their definitions in `_moul` and their tables.
//!
//! Every function here runs on a connection or transaction the caller
//! holds, so a definition change and the table change it needs commit
//! together.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{Collection, CollectionKind, EmailTemplates, FieldChanges, FieldType, quote};
use crate::{Error, Result};

/// Reads every collection definition, in creation order.
pub fn load_all(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, type, fields, rules, email_templates, webhooks, createdAt, updatedAt
         FROM main._moul ORDER BY createdAt, rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(StoredRow {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            fields: r.get(3)?,
            rules: r.get(4)?,
            email_templates: r.get(5)?,
            webhooks: r.get(6)?,
            created_at: r.get(7)?,
            updated_at: r.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?.parse()?);
    }
    Ok(out)
}

/// A counter that grows with every change to `_moul`, kept by triggers. A
/// cache of definitions is fresh while this number is unchanged, including
/// after pages from another device were applied underneath SQLite.
pub fn version(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT version FROM main._moul_version WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

struct StoredRow {
    id: String,
    name: String,
    kind: String,
    fields: String,
    rules: String,
    email_templates: Option<String>,
    webhooks: Option<String>,
    created_at: String,
    updated_at: String,
}

impl StoredRow {
    fn parse(self) -> Result<Collection> {
        let bad = |what: &str, e: serde_json::Error| {
            Error::Corrupt(format!(
                "{what} of collection {:?} cannot be read: {e}",
                self.name
            ))
        };
        let fields = if self.fields.trim().is_empty() || self.fields == "null" {
            Vec::new()
        } else {
            serde_json::from_str(&self.fields).map_err(|e| bad("fields", e))?
        };
        let rules = serde_json::from_str(&self.rules).map_err(|e| bad("rules", e))?;
        let kind = CollectionKind::parse(&self.kind);
        // As in Go: unreadable templates or webhooks are ignored, and an auth
        // collection without templates gets the defaults.
        let mut email_templates = self
            .email_templates
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "{}" && *s != "null")
            .and_then(|s| serde_json::from_str::<EmailTemplates>(s).ok());
        if kind == CollectionKind::Auth && email_templates.is_none() {
            email_templates = Some(EmailTemplates::default());
        }
        if kind != CollectionKind::Auth {
            email_templates = None;
        }
        let webhooks = self
            .webhooks
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "[]" && *s != "null")
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        Ok(Collection {
            id: self.id,
            name: self.name,
            kind,
            fields,
            rules,
            email_templates,
            webhooks,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Inserts or replaces the definition of `c`, keyed by its id.
///
/// A webhook secret never reaches the shared store: any `secret` key of a
/// webhook (in any case) is left out, and a non-empty one is kept in
/// `writer._webhook_secrets` instead (see [`move_webhook_secrets`]).
pub fn save(conn: &Connection, c: &Collection) -> Result<()> {
    let mut hooks = c.webhooks.clone();
    move_webhook_secrets(conn, &c.id, &mut hooks)?;
    let fields = serde_json::to_string(&c.fields).expect("fields serialise");
    let rules = serde_json::to_string(&c.rules).expect("rules serialise");
    let templates = match (&c.kind, &c.email_templates) {
        (CollectionKind::Auth, Some(t)) => serde_json::to_string(t).expect("templates serialise"),
        (CollectionKind::Auth, None) => {
            serde_json::to_string(&EmailTemplates::default()).expect("templates serialise")
        }
        _ => "{}".to_owned(),
    };
    let webhooks = serde_json::to_string(&hooks).expect("webhooks serialise");
    conn.execute(
        "INSERT INTO main._moul (id, name, type, fields, rules, email_templates, webhooks, createdAt, updatedAt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, type = excluded.type,
           fields = excluded.fields, rules = excluded.rules,
           email_templates = excluded.email_templates, webhooks = excluded.webhooks,
           updatedAt = excluded.updatedAt",
        params![
            c.id,
            c.name,
            c.kind.as_str(),
            fields,
            rules,
            templates,
            webhooks,
            c.created_at,
            c.updated_at
        ],
    )?;
    Ok(())
}

/// Takes the secrets out of `webhooks`: removes every key named `secret`,
/// compared without case as Go's JSON decoding does, and keeps a non-empty
/// string secret in `writer._webhook_secrets`, keyed by `collection_id`
/// and the webhook's `id`. A webhook given without a secret keeps the one
/// stored before. A webhook without a non-empty string `id` loses its
/// secret: nothing could find it again, and two such webhooks would
/// overwrite each other's. Returns the secrets stored.
pub fn move_webhook_secrets(
    conn: &Connection,
    collection_id: &str,
    webhooks: &mut [serde_json::Value],
) -> Result<usize> {
    let mut stored = 0;
    for hook in webhooks {
        let Some(obj) = hook.as_object_mut() else {
            continue;
        };
        let keys: Vec<String> = obj
            .keys()
            .filter(|k| k.eq_ignore_ascii_case("secret"))
            .cloned()
            .collect();
        // If several spellings are given, the exact `secret` wins.
        let mut secret = None;
        for key in keys {
            let value = obj.remove(&key);
            if let Some(s) = value
                .as_ref()
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                && (secret.is_none() || key == "secret")
            {
                secret = Some(s.to_owned());
            }
        }
        let Some(secret) = secret else {
            continue;
        };
        let Some(id) = obj
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        conn.execute(
            "INSERT OR REPLACE INTO writer._webhook_secrets (collection, webhook, secret) VALUES (?1, ?2, ?3)",
            params![collection_id, id, secret],
        )?;
        stored += 1;
    }
    Ok(stored)
}

/// Removes the definition with id `id`.
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM main._moul WHERE id = ?1", [id])?;
    Ok(())
}

/// The column definitions of the system columns of `kind`, as the Go engine
/// declares them. Auth collections keep only `username` and `email` here;
/// their credentials live in the writer-only store.
fn system_columns_sql(kind: CollectionKind) -> &'static str {
    match kind {
        CollectionKind::Base => {
            "id TEXT PRIMARY KEY,\n  createdAt TEXT NOT NULL,\n  updatedAt TEXT NOT NULL"
        }
        CollectionKind::Auth => {
            "id TEXT PRIMARY KEY,\n  createdAt TEXT NOT NULL,\n  updatedAt TEXT NOT NULL,\n  \
             username TEXT UNIQUE NOT NULL,\n  email TEXT UNIQUE NOT NULL"
        }
        CollectionKind::Worker => {
            "id TEXT PRIMARY KEY,\n  createdAt TEXT NOT NULL,\n  updatedAt TEXT NOT NULL,\n  \
             state TEXT NOT NULL DEFAULT 'available',\n  queue TEXT NOT NULL DEFAULT 'default',\n  \
             worker TEXT NOT NULL,\n  args TEXT NOT NULL DEFAULT '{}',\n  meta TEXT NOT NULL DEFAULT '{}',\n  \
             tags TEXT NOT NULL DEFAULT '[]',\n  errors TEXT NOT NULL DEFAULT '[]',\n  \
             attempt INTEGER NOT NULL DEFAULT 0,\n  max_attempts INTEGER NOT NULL DEFAULT 20,\n  \
             priority INTEGER NOT NULL DEFAULT 0,\n  inserted_at TEXT NOT NULL,\n  scheduled_at TEXT NOT NULL,\n  \
             attempted_at TEXT,\n  attempted_by TEXT,\n  cancelled_at TEXT,\n  completed_at TEXT,\n  discarded_at TEXT"
        }
        CollectionKind::Analytic => {
            "id TEXT PRIMARY KEY,\n  createdAt TEXT NOT NULL,\n  updatedAt TEXT NOT NULL,\n  \
             visit_token TEXT NOT NULL,\n  visitor_token TEXT NOT NULL,\n  user_id TEXT,\n  \
             name TEXT NOT NULL,\n  properties TEXT NOT NULL DEFAULT '{}',\n  time TEXT NOT NULL"
        }
    }
}

fn create_table_sql(table: &str, c: &Collection) -> String {
    let mut sql = format!(
        "CREATE TABLE main.{} (\n  {}",
        quote(table),
        system_columns_sql(c.kind)
    );
    for f in c.column_fields() {
        sql += &format!(",\n  {} {}", quote(&f.name), f.kind.column_type());
        if f.kind == FieldType::Cloak && f.searchable {
            sql += &format!(",\n  {} TEXT", quote(&format!("{}Hash", f.name)));
        }
    }
    sql + "\n)"
}

fn unique_index_name(c: &Collection, field: &str) -> String {
    format!("idx_{}_{}_unique", c.name, field)
}

/// The indexes `c` should have besides those SQLite makes for constraints:
/// name and `CREATE INDEX` statement.
fn wanted_indexes(c: &Collection) -> Vec<(String, String)> {
    let table = quote(&c.name);
    let mut out = Vec::new();
    if c.kind == CollectionKind::Worker {
        let name = format!("idx_{}_job_processing", c.name);
        out.push((
            name.clone(),
            format!(
                "CREATE INDEX main.{} ON {table} (state, queue, priority, scheduled_at, id)",
                quote(&name)
            ),
        ));
    }
    for f in c.column_fields() {
        if f.unique {
            let name = unique_index_name(c, &f.name);
            out.push((
                name.clone(),
                format!(
                    "CREATE UNIQUE INDEX main.{} ON {table} ({})",
                    quote(&name),
                    quote(&f.name)
                ),
            ));
        }
        if f.kind == FieldType::Cloak && f.searchable {
            let name = format!("idx_{}_{}_hash", c.name, f.name);
            out.push((
                name.clone(),
                format!(
                    "CREATE INDEX main.{} ON {table} ({})",
                    quote(&name),
                    quote(&format!("{}Hash", f.name))
                ),
            ));
        }
    }
    out
}

/// The indexes on `table` that Moul manages: every named index that is not
/// SQLite's own (`sqlite_autoindex_…`).
fn existing_indexes(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM main.sqlite_schema
         WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL",
    )?;
    let names = stmt
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

/// Brings the indexes of `c` in line with its definition.
fn sync_indexes(conn: &Connection, c: &Collection) -> Result<()> {
    let wanted = wanted_indexes(c);
    let wanted_names: HashSet<&str> = wanted.iter().map(|(n, _)| n.as_str()).collect();
    let existing = existing_indexes(conn, &c.name)?;
    for name in &existing {
        if !wanted_names.contains(name.as_str()) {
            conn.execute_batch(&format!("DROP INDEX main.{}", quote(name)))?;
        }
    }
    for (name, sql) in &wanted {
        if existing.iter().any(|e| e == name) {
            continue;
        }
        conn.execute_batch(sql)
            .map_err(|e| unique_error(c, name, e))?;
    }
    Ok(())
}

fn unique_error(c: &Collection, index: &str, e: rusqlite::Error) -> Error {
    let text = e.to_string();
    if text.contains("UNIQUE constraint failed") || text.contains("not unique") {
        let field = c
            .column_fields()
            .find(|f| unique_index_name(c, &f.name) == index)
            .map(|f| f.name.as_str())
            .unwrap_or(index);
        return Error::Conflict(format!(
            "cannot enable unique constraint on field {field:?}: existing records contain duplicates"
        ));
    }
    Error::Sqlite(e)
}

/// Creates the table of a new collection, with its indexes.
pub fn create_table(conn: &Connection, c: &Collection) -> Result<()> {
    conn.execute_batch(&create_table_sql(&c.name, c))?;
    sync_indexes(conn, c)
}

/// Drops the table of a collection.
pub fn drop_table(conn: &Connection, c: &Collection) -> Result<()> {
    conn.execute_batch(&format!("DROP TABLE IF EXISTS main.{}", quote(&c.name)))?;
    Ok(())
}

/// Whether a table named `name` exists in the shared store.
pub fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM main.sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[derive(Debug)]
struct ColumnInfo {
    name: String,
    decl: String,
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>> {
    let mut stmt = conn.prepare(&format!("PRAGMA main.table_info({})", quote(table)))?;
    let cols = stmt
        .query_map([], |r| {
            Ok(ColumnInfo {
                name: r.get(1)?,
                decl: r.get::<_, String>(2)?.to_ascii_uppercase(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols)
}

/// Changes the table of `old` into the table of `new`: renames the table,
/// renames columns whose field kept its id, adds and drops columns, and
/// rebuilds the table when a column's type changes. Fields are matched by
/// id, so both definitions must have them.
///
/// Columns of removed fields are dropped before any rename, and renames go
/// through temporary names, so a chain (`title` to `name` while `name`
/// becomes `fullName`), a swap, or taking a removed field's name all keep
/// each column's data with its field.
///
/// Relation fields elsewhere that point at a renamed collection are not
/// changed here; see [`retarget_relations`].
pub fn sync_table(conn: &Connection, old: &Collection, new: &Collection) -> Result<()> {
    if old.name != new.name {
        if old.name.eq_ignore_ascii_case(&new.name) {
            // SQLite compares names without case: go through a spare name.
            let spare = format!("_moul_rename_{}", new.name);
            rename_table(conn, &old.name, &spare)?;
            rename_table(conn, &spare, &new.name)?;
        } else {
            rename_table(conn, &old.name, &new.name)?;
        }
    }
    if !table_exists(conn, &new.name)? {
        return create_table(conn, new);
    }

    let changes = FieldChanges::between(old, new);
    // Indexes that name a column must go before the column is renamed or
    // dropped; sync_indexes puts back the ones the new definition wants.
    for name in existing_indexes(conn, &new.name)? {
        conn.execute_batch(&format!("DROP INDEX main.{}", quote(&name)))?;
    }

    // Removed fields first, so their names are free for renames.
    let present = columns(conn, &new.name)?;
    let has = |name: &str| present.iter().any(|c| c.name.eq_ignore_ascii_case(name));
    for id in &changes.removed {
        let Some(f) = old.field_by_id(id) else {
            continue;
        };
        if old.is_system_column(&f.name) {
            continue;
        }
        let mut gone = vec![f.name.clone()];
        if f.kind == FieldType::Cloak && f.searchable {
            gone.push(format!("{}Hash", f.name));
        }
        for col in gone {
            if has(&col) {
                drop_column(conn, &new.name, &col)?;
            }
        }
    }

    // Renames in two steps: every column moves to a name of its field id,
    // then to its new name.
    let mut moves = Vec::new();
    for (id, from, to) in &changes.renamed {
        if old.is_system_column(from) || new.is_system_column(to) || !has(from) {
            continue;
        }
        moves.push((from.clone(), format!("__moul_{id}"), to.clone()));
        let was = old.field_by_id(id);
        if was.is_some_and(|f| f.kind == FieldType::Cloak && f.searchable)
            && has(&format!("{from}Hash"))
        {
            moves.push((
                format!("{from}Hash"),
                format!("__moul_{id}_hash"),
                format!("{to}Hash"),
            ));
        }
    }
    for (from, temp, _) in &moves {
        rename_column(conn, &new.name, from, temp)?;
    }
    for (_, temp, to) in &moves {
        rename_column(conn, &new.name, temp, to)?;
    }

    let existing = columns(conn, &new.name)?;
    let find = |name: &str| existing.iter().find(|c| c.name.eq_ignore_ascii_case(name));

    let needs_rebuild = new.column_fields().any(|f| {
        find(&f.name).is_some_and(|col| col.decl != f.kind.column_type())
            || (f.kind == FieldType::Cloak
                && f.searchable
                && find(&format!("{}Hash", f.name)).is_some_and(|col| col.decl != "TEXT"))
    });
    if needs_rebuild {
        rebuild_table(conn, new, &existing)?;
        return sync_indexes(conn, new);
    }

    for f in new.column_fields() {
        if find(&f.name).is_none() {
            conn.execute_batch(&format!(
                "ALTER TABLE main.{} ADD COLUMN {} {}",
                quote(&new.name),
                quote(&f.name),
                f.kind.column_type()
            ))?;
        }
        if f.kind == FieldType::Cloak && f.searchable {
            let hash = format!("{}Hash", f.name);
            if find(&hash).is_none() {
                conn.execute_batch(&format!(
                    "ALTER TABLE main.{} ADD COLUMN {} TEXT",
                    quote(&new.name),
                    quote(&hash)
                ))?;
            }
        }
    }

    // Anything else that no field owns.
    let mut keep: HashSet<String> = HashSet::new();
    for f in new.column_fields() {
        keep.insert(f.name.to_ascii_lowercase());
        if f.kind == FieldType::Cloak && f.searchable {
            keep.insert(format!("{}hash", f.name.to_ascii_lowercase()));
        }
    }
    for col in &existing {
        let lower = col.name.to_ascii_lowercase();
        if new.is_system_column(&col.name) || keep.contains(&lower) {
            continue;
        }
        drop_column(conn, &new.name, &col.name)?;
    }
    sync_indexes(conn, new)
}

fn rename_table(conn: &Connection, from: &str, to: &str) -> Result<()> {
    conn.execute_batch(&format!(
        "ALTER TABLE main.{} RENAME TO {}",
        quote(from),
        quote(to)
    ))?;
    Ok(())
}

fn drop_column(conn: &Connection, table: &str, column: &str) -> Result<()> {
    conn.execute_batch(&format!(
        "ALTER TABLE main.{} DROP COLUMN {}",
        quote(table),
        quote(column)
    ))?;
    Ok(())
}

fn rename_column(conn: &Connection, table: &str, from: &str, to: &str) -> Result<()> {
    conn.execute_batch(&format!(
        "ALTER TABLE main.{} RENAME COLUMN {} TO {}",
        quote(table),
        quote(from),
        quote(to)
    ))?;
    Ok(())
}

fn cast_expression(column: &str, kind: &FieldType) -> String {
    match kind {
        FieldType::Number => format!("CAST({column} AS NUMERIC)"),
        FieldType::Bool => {
            format!("CASE WHEN {column} IN ('1', 1, 'true', 'TRUE', 't', 'T') THEN 1 ELSE 0 END")
        }
        _ => format!("CAST({column} AS TEXT)"),
    }
}

/// Recreates the table of `c` with the right column types, copying rows
/// and converting values the way the Go engine does.
fn rebuild_table(conn: &Connection, c: &Collection, existing: &[ColumnInfo]) -> Result<()> {
    let temp = format!("_moul_rebuild_{}", c.name);
    conn.execute_batch(&create_table_sql(&temp, c))?;
    let find = |name: &str| {
        existing
            .iter()
            .find(|col| col.name.eq_ignore_ascii_case(name))
    };
    let mut into = Vec::new();
    let mut select = Vec::new();
    for col in existing {
        if c.is_system_column(&col.name) {
            into.push(quote(&col.name));
            select.push(quote(&col.name));
        }
    }
    for f in c.column_fields() {
        if let Some(col) = find(&f.name) {
            into.push(quote(&f.name));
            if col.decl == f.kind.column_type() {
                select.push(quote(&col.name));
            } else {
                select.push(cast_expression(&quote(&col.name), &f.kind));
            }
        }
        if f.kind == FieldType::Cloak && f.searchable {
            let hash = format!("{}Hash", f.name);
            if let Some(col) = find(&hash) {
                into.push(quote(&hash));
                select.push(quote(&col.name));
            }
        }
    }
    if !into.is_empty() {
        conn.execute_batch(&format!(
            "INSERT INTO main.{} ({}) SELECT {} FROM main.{}",
            quote(&temp),
            into.join(", "),
            select.join(", "),
            quote(&c.name)
        ))?;
    }
    conn.execute_batch(&format!(
        "DROP TABLE main.{};
         ALTER TABLE main.{} RENAME TO {};",
        quote(&c.name),
        quote(&temp),
        quote(&c.name)
    ))?;
    Ok(())
}

/// Points relation fields of other collections at `new_name` after the
/// collection `old_name` was renamed, and saves the definitions that
/// changed. Returns the changed collections.
pub fn retarget_relations(
    conn: &Connection,
    old_name: &str,
    new_name: &str,
    updated_at: &str,
) -> Result<Vec<Collection>> {
    let mut changed = Vec::new();
    for mut c in load_all(conn)? {
        let mut touched = false;
        for f in &mut c.fields {
            if let Some(rel) = f.relation.as_mut()
                && rel.target_moul == old_name
            {
                rel.target_moul = new_name.to_owned();
                touched = true;
            }
        }
        if touched {
            c.updated_at = updated_at.to_owned();
            save(conn, &c)?;
            changed.push(c);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::schema::{Field, Rules};

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.migrate(|| unreachable!("a new database needs no ids"))
            .unwrap();
        s
    }

    fn posts() -> Collection {
        let mut c = Collection::new("posts", CollectionKind::Base);
        c.id = "c1".into();
        let mut title = Field::new("title", FieldType::Text);
        title.id = "fld_1".into();
        let mut score = Field::new("score", FieldType::Text);
        score.id = "fld_2".into();
        c.fields = vec![title, score];
        c.rules = Rules::default();
        c.created_at = "2026-11-18T00:00:00Z".into();
        c.updated_at = c.created_at.clone();
        c
    }

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        columns(conn, table)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn save_load_and_version() {
        let mut s = store();
        let v0 = version(s.conn()).unwrap();
        s.write(|tx| {
            save(tx, &posts())?;
            create_table(tx, &posts())
        })
        .unwrap();
        let all = load_all(s.conn()).unwrap();
        assert_eq!(all, vec![posts()]);
        assert!(version(s.conn()).unwrap() > v0);
    }

    #[test]
    fn renames_keep_data_and_type_changes_rebuild() {
        let mut s = store();
        let old = posts();
        s.write(|tx| {
            save(tx, &old)?;
            create_table(tx, &old)?;
            tx.execute(
                "INSERT INTO posts (id, createdAt, updatedAt, title, score) VALUES ('p1', 'x', 'x', 'Hi', '42')",
                [],
            )?;
            Ok::<_, Error>(())
        })
        .unwrap();

        // Rename the table and a column, and make `score` a unique number.
        let mut new = old.clone();
        new.name = "articles".into();
        new.fields[0].name = "headline".into();
        new.fields[1].kind = FieldType::Number;
        new.fields[1].unique = true;
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();

        let conn = s.conn();
        assert_eq!(
            column_names(conn, "articles"),
            ["id", "createdAt", "updatedAt", "headline", "score"]
        );
        let (headline, score): (String, i64) = conn
            .query_row(
                "SELECT headline, score FROM articles WHERE id = 'p1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((headline.as_str(), score), ("Hi", 42));
        assert_eq!(
            existing_indexes(conn, "articles").unwrap(),
            ["idx_articles_score_unique"]
        );
        assert!(!table_exists(conn, "posts").unwrap());
    }

    #[test]
    fn drops_and_adds_columns() {
        let mut s = store();
        let old = posts();
        s.write(|tx| create_table(tx, &old)).unwrap();
        let mut new = old.clone();
        new.fields.remove(1);
        let mut tags = Field::new("tags", FieldType::Json);
        tags.id = "fld_3".into();
        new.fields.push(tags);
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        assert_eq!(
            column_names(s.conn(), "posts"),
            ["id", "createdAt", "updatedAt", "title", "tags"]
        );
    }

    #[test]
    fn unique_on_duplicates_is_a_conflict() {
        let mut s = store();
        let old = posts();
        s.write(|tx| {
            create_table(tx, &old)?;
            tx.execute_batch(
                "INSERT INTO posts (id, createdAt, updatedAt, title) VALUES ('a', 'x', 'x', 'same'), ('b', 'x', 'x', 'same')",
            )?;
            Ok::<_, Error>(())
        })
        .unwrap();
        let mut new = old.clone();
        new.fields[0].unique = true;
        let err = s.write(|tx| sync_table(tx, &old, &new)).unwrap_err();
        assert!(
            matches!(err, Error::Conflict(ref m) if m.contains("\"title\"")),
            "{err}"
        );
    }

    #[test]
    fn case_only_rename() {
        let mut s = store();
        let old = posts();
        s.write(|tx| create_table(tx, &old)).unwrap();
        let mut new = old.clone();
        new.fields[0].name = "tItle".into();
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        assert_eq!(column_names(s.conn(), "posts")[3], "tItle");
    }

    fn insert(s: &mut Store, table: &str, cols: &[&str], rows: &[(&str, &[&str])]) {
        s.write(|tx| {
            for (id, values) in rows {
                let mut names = vec!["id", "createdAt", "updatedAt"];
                names.extend_from_slice(cols);
                let mut all = vec![*id, "x", "x"];
                all.extend_from_slice(values);
                let marks = vec!["?"; all.len()].join(", ");
                let names: Vec<String> = names.iter().map(|n| quote(n)).collect();
                tx.execute(
                    &format!(
                        "INSERT INTO {} ({}) VALUES ({marks})",
                        quote(table),
                        names.join(", ")
                    ),
                    rusqlite::params_from_iter(all.iter()),
                )?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    fn read(s: &Store, table: &str, cols: &[&str], id: &str) -> Vec<Option<String>> {
        let names: Vec<String> = cols.iter().map(|n| quote(n)).collect();
        s.conn()
            .query_row(
                &format!(
                    "SELECT {} FROM {} WHERE id = ?1",
                    names.join(", "),
                    quote(table)
                ),
                [id],
                |r| (0..cols.len()).map(|i| r.get(i)).collect(),
            )
            .unwrap()
    }

    fn some(values: &[&str]) -> Vec<Option<String>> {
        values.iter().map(|v| Some((*v).to_owned())).collect()
    }

    fn people() -> Collection {
        let mut c = Collection::new("people", CollectionKind::Base);
        c.id = "c2".into();
        for (i, name) in ["title", "name", "headline"].into_iter().enumerate() {
            let mut f = Field::new(name, FieldType::Text);
            f.id = format!("fld_{}", i + 1);
            c.fields.push(f);
        }
        c
    }

    fn people_store() -> (Store, Collection) {
        let mut s = store();
        let old = people();
        s.write(|tx| create_table(tx, &old)).unwrap();
        insert(
            &mut s,
            "people",
            &["title", "name", "headline"],
            &[("p1", &["T", "N", "H"])],
        );
        (s, old)
    }

    #[test]
    fn a_rename_chain_keeps_data() {
        let (mut s, old) = people_store();
        // title -> name while name -> fullName.
        let mut new = old.clone();
        new.fields[0].name = "name".into();
        new.fields[1].name = "fullName".into();
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        assert_eq!(
            column_names(s.conn(), "people"),
            [
                "id",
                "createdAt",
                "updatedAt",
                "name",
                "fullName",
                "headline"
            ]
        );
        assert_eq!(
            read(&s, "people", &["name", "fullName", "headline"], "p1"),
            some(&["T", "N", "H"])
        );
    }

    #[test]
    fn a_rename_swap_keeps_data() {
        let (mut s, old) = people_store();
        let mut new = old.clone();
        new.fields[0].name = "name".into();
        new.fields[1].name = "title".into();
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        assert_eq!(
            read(&s, "people", &["title", "name", "headline"], "p1"),
            some(&["N", "T", "H"])
        );
    }

    #[test]
    fn a_rename_onto_a_removed_field_keeps_data() {
        let (mut s, old) = people_store();
        // headline is deleted and title takes its name, in one update.
        let mut new = old.clone();
        new.fields.remove(2);
        new.fields[0].name = "headline".into();
        new.fields[0].unique = true;
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        assert_eq!(
            column_names(s.conn(), "people"),
            ["id", "createdAt", "updatedAt", "headline", "name"]
        );
        assert_eq!(
            read(&s, "people", &["headline", "name"], "p1"),
            some(&["T", "N"])
        );
        assert_eq!(
            existing_indexes(s.conn(), "people").unwrap(),
            ["idx_people_headline_unique"]
        );
    }

    #[test]
    fn a_renamed_cloak_keeps_its_hash() {
        let mut s = store();
        let mut old = Collection::new("vault", CollectionKind::Base);
        let mut secret = Field::new("secret", FieldType::Cloak);
        secret.id = "fld_1".into();
        secret.searchable = true;
        old.fields = vec![secret];
        s.write(|tx| create_table(tx, &old)).unwrap();
        insert(
            &mut s,
            "vault",
            &["secret", "secretHash"],
            &[("v1", &["S", "H"])],
        );
        let mut new = old.clone();
        new.fields[0].name = "code".into();
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        assert_eq!(
            read(&s, "vault", &["code", "codeHash"], "v1"),
            some(&["S", "H"])
        );
    }

    #[test]
    fn a_case_only_collection_rename_keeps_data() {
        let mut s = store();
        let mut old = posts();
        old.fields[0].unique = true;
        s.write(|tx| create_table(tx, &old)).unwrap();
        insert(&mut s, "posts", &["title"], &[("p1", &["Hi"])]);
        let mut new = old.clone();
        new.name = "Posts".into();
        s.write(|tx| sync_table(tx, &old, &new)).unwrap();
        let name: String = s
            .conn()
            .query_row(
                "SELECT name FROM main.sqlite_schema WHERE type = 'table' AND name = 'Posts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "Posts");
        assert_eq!(read(&s, "Posts", &["title"], "p1"), some(&["Hi"]));
        assert_eq!(
            existing_indexes(s.conn(), "Posts").unwrap(),
            ["idx_Posts_title_unique"]
        );
    }

    #[test]
    fn save_moves_webhook_secrets_to_the_writer_store() {
        let mut s = store();
        let mut c = posts();
        c.webhooks = vec![
            serde_json::json!({"id": "w1", "url": "https://h.example/x", "secret": "topsecret"}),
            serde_json::json!({"id": "w2", "url": "https://h.example/y", "SECRET": "othersecret"}),
            serde_json::json!({"id": "w3", "url": "https://h.example/z", "secret": ""}),
            // Without an id, a secret cannot be found again: it is dropped,
            // and two such webhooks do not overwrite each other's.
            serde_json::json!({"url": "https://h.example/a", "secret": "noid1"}),
            serde_json::json!({"id": "", "url": "https://h.example/b", "secret": "noid2"}),
            serde_json::json!({"id": 7, "url": "https://h.example/c", "secret": "noid3"}),
        ];
        s.write(|tx| save(tx, &c)).unwrap();

        let stored: String = s
            .conn()
            .query_row("SELECT webhooks FROM main._moul WHERE id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!stored.to_ascii_lowercase().contains("secret"), "{stored}");
        let loaded = &load_all(s.conn()).unwrap()[0];
        assert_eq!(loaded.webhooks.len(), 6);
        assert_eq!(loaded.webhooks[0]["url"], "https://h.example/x");
        assert_eq!(loaded.webhooks[3]["url"], "https://h.example/a");

        let secrets: Vec<(String, String)> = s
            .conn()
            .prepare(
                "SELECT webhook, secret FROM writer._webhook_secrets
                 WHERE collection = 'c1' ORDER BY webhook",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            secrets,
            [
                ("w1".to_owned(), "topsecret".to_owned()),
                ("w2".to_owned(), "othersecret".to_owned())
            ]
        );
        let file = s.export().unwrap();
        for needle in [&b"topsecret"[..], b"othersecret", b"noid"] {
            assert!(!file.windows(needle.len()).any(|w| w == needle));
        }
    }

    #[test]
    fn stored_templates_read_like_go() {
        let mut s = store();
        let mut users = Collection::new("users", CollectionKind::Auth);
        users.id = "u".into();
        users.validate(None).unwrap();
        s.write(|tx| save(tx, &users)).unwrap();
        assert_eq!(load_all(s.conn()).unwrap(), vec![users.clone()]);

        // A stored row with some templates: the others are empty, as in Go.
        s.conn()
            .execute(
                "UPDATE main._moul SET email_templates = '{\"otp\":{\"subject\":\"S\",\"body\":\"B\"}}'",
                [],
            )
            .unwrap();
        let t = load_all(s.conn()).unwrap()[0]
            .email_templates
            .clone()
            .unwrap();
        assert_eq!((t.otp.subject.as_str(), t.otp.body.as_str()), ("S", "B"));
        assert_eq!(t.password_reset, Default::default());

        // No templates at all: the built-in ones.
        s.conn()
            .execute("UPDATE main._moul SET email_templates = '{}'", [])
            .unwrap();
        assert_eq!(
            load_all(s.conn()).unwrap()[0].email_templates,
            Some(EmailTemplates::default())
        );
    }

    #[test]
    fn auth_tables_hold_no_credentials() {
        let mut s = store();
        let mut users = Collection::new("users", CollectionKind::Auth);
        users.id = "u".into();
        s.write(|tx| create_table(tx, &users)).unwrap();
        assert_eq!(
            column_names(s.conn(), "users"),
            ["id", "createdAt", "updatedAt", "username", "email"]
        );
    }
}
