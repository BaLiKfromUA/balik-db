//! Persistent catalog of tables for a column-store database.
//!
//! On-disk layout (rooted at the database directory):
//!
//! ```text
//!   catalog.toml            # this module's top-level index of tables
//!   tables/
//!     00000001/
//!       manifest.toml       # this table's schema + layout
//!       row_groups/         # populated by inserts in later stages
//! ```
//!
//! Catalog updates are atomic: write `catalog.toml.tmp` -> fsync -> rename.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::schema::{Column, ColumnType, Schema};
use crate::checksum;
use crate::error::Error;

pub type TableId = u64;

const CATALOG_FILE: &str = "catalog.toml";
const CATALOG_TMP_FILE: &str = "catalog.toml.tmp";
const MANIFEST_FILE: &str = "manifest.toml";
const TABLES_DIR: &str = "tables";
const STORAGE_TRACK: &str = "column-store";
const FORMAT_VERSION: u32 = 1;

pub const DEFAULT_ROW_GROUP_SIZE: u32 = 8192;

#[derive(Debug, Clone, Default)]
pub struct TableOptions {
    pub row_group_size: Option<u32>,
}

/// Fully-loaded view of a single table — produced by `describe_table` /
/// `open_table`.
#[derive(Debug, Clone)]
pub struct TableDescriptor {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
    pub row_group_size: u32,
    pub storage_track: String,
    /// Absolute path to the table directory. `open_table` forwards this into
    /// the `TableHandle` so the data plane can locate row-group files.
    pub dir: PathBuf,
    /// Next record id to assign. The insert path reads and advances this.
    pub next_rid: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CatalogFile {
    format_version: u32,
    storage_track: String,
    next_table_id: u64,
    #[serde(default)]
    tables: Vec<TableEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TableEntry {
    id: TableId,
    name: String,
    /// Relative path from the db root, using forward slashes for portability.
    dir: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ManifestFile {
    format_version: u32,
    table_id: TableId,
    name: String,
    storage_track: String,
    row_group_size: u32,
    next_rid: u64,
    columns: Vec<ManifestColumn>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ManifestColumn {
    name: String,
    #[serde(rename = "type")]
    ty: ColumnType,
    nullable: bool,
    /// Templated path, relative to the table dir. `{row_group}` is substituted
    /// with the zero-padded row-group id at insert time (Stage 2+).
    file: String,
}

#[derive(Debug)]
pub struct Catalog {
    root: PathBuf,
    tables: BTreeMap<String, TableEntry>,
    next_table_id: u64,
}

impl Catalog {
    /// Open the catalog rooted at `db_root`. If `catalog.toml` does not yet
    /// exist (fresh `init`), an empty in-memory catalog is returned; it is
    /// materialized on the first `create_table`.
    pub fn load(db_root: &Path) -> Result<Self, Error> {
        let path = db_root.join(CATALOG_FILE);
        if !path.exists() {
            tracing::debug!(path = %path.display(), "no catalog file, starting empty");
            return Ok(Self {
                root: db_root.to_path_buf(),
                tables: BTreeMap::new(),
                next_table_id: 1,
            });
        }
        tracing::debug!(path = %path.display(), "loading catalog");
        let bytes = fs::read(&path).map_err(|e| Error::io("read catalog.toml", e))?;
        let body = checksum::verify(&bytes)
            .map_err(|e| Error::invalid_schema(format!("catalog.toml: {e}")))?;
        let content = std::str::from_utf8(body)
            .map_err(|_| Error::invalid_schema("catalog.toml is not valid UTF-8".to_string()))?;
        let file: CatalogFile = toml::from_str(content)
            .map_err(|e| Error::invalid_schema(format!("catalog.toml: {e}")))?;
        if file.storage_track != STORAGE_TRACK {
            return Err(Error::invalid_schema(format!(
                "catalog storage_track is '{}', expected '{STORAGE_TRACK}'",
                file.storage_track
            )));
        }
        if file.format_version > FORMAT_VERSION {
            return Err(Error::invalid_schema(format!(
                "catalog format_version {} is newer than supported ({FORMAT_VERSION})",
                file.format_version
            )));
        }
        let mut tables = BTreeMap::new();
        for entry in file.tables {
            tables.insert(entry.name.clone(), entry);
        }
        tracing::debug!(
            tables = tables.len(),
            next_table_id = file.next_table_id,
            "catalog loaded"
        );
        Ok(Self {
            root: db_root.to_path_buf(),
            tables,
            next_table_id: file.next_table_id,
        })
    }

    /// Persist `catalog.toml` atomically: write to a temp file, fsync, rename.
    fn save_atomic(&self) -> Result<(), Error> {
        let file = CatalogFile {
            format_version: FORMAT_VERSION,
            storage_track: STORAGE_TRACK.to_string(),
            next_table_id: self.next_table_id,
            tables: self.tables.values().cloned().collect(),
        };
        let serialized = toml::to_string(&file)
            .map_err(|e| Error::invalid_schema(format!("serialize catalog: {e}")))?;
        let wrapped = checksum::wrap(serialized.as_bytes());
        let tmp_path = self.root.join(CATALOG_TMP_FILE);
        let final_path = self.root.join(CATALOG_FILE);
        tracing::debug!(path = %tmp_path.display(), bytes = wrapped.len(), "writing catalog tmp");
        fs::write(&tmp_path, &wrapped).map_err(|e| Error::io("write catalog tmp", e))?;
        tracing::debug!(path = %tmp_path.display(), "fsync catalog tmp");
        fs::File::open(&tmp_path)
            .and_then(|f| f.sync_all())
            .map_err(|e| Error::io("fsync catalog tmp", e))?;
        tracing::debug!(from = %tmp_path.display(), to = %final_path.display(), "renaming catalog into place");
        fs::rename(&tmp_path, &final_path).map_err(|e| Error::io("rename catalog", e))?;
        Ok(())
    }

    pub fn create_table(
        &mut self,
        name: &str,
        schema: Schema,
        options: TableOptions,
    ) -> Result<TableId, Error> {
        tracing::info!(table = %name, columns = schema.columns.len(), "creating table");
        schema.validate(name)?;
        if self.tables.contains_key(name) {
            return Err(Error::table_exists(name));
        }

        let id = self.next_table_id;
        // Forward-slash form lives in catalog.toml; only converted to PathBuf for fs ops.
        let dir_rel = format!("{TABLES_DIR}/{id:08}");
        let dir_abs = self.root.join(TABLES_DIR).join(format!("{id:08}"));

        // Filesystem layout first; catalog publish happens last so a crash
        // mid-create leaves an orphan dir, never a dangling catalog entry.
        // Storage-track-specific layout (row groups for column store, heap
        // for row store, etc.) is materialized by the caller after this
        // function returns.
        tracing::debug!(table_id = id, path = %dir_abs.display(), "creating table dir");
        fs::create_dir_all(&dir_abs).map_err(|e| Error::io("create table dir", e))?;

        let row_group_size = options.row_group_size.unwrap_or(DEFAULT_ROW_GROUP_SIZE);
        let manifest = ManifestFile {
            format_version: FORMAT_VERSION,
            table_id: id,
            name: name.to_string(),
            storage_track: STORAGE_TRACK.to_string(),
            row_group_size,
            next_rid: 0,
            columns: schema
                .columns
                .iter()
                .map(|c| ManifestColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                    nullable: c.nullable,
                    file: format!("{{row_group}}/{}.col", c.name),
                })
                .collect(),
        };
        let manifest_path = dir_abs.join(MANIFEST_FILE);
        tracing::debug!(path = %manifest_path.display(), "writing manifest");
        Self::write_manifest_atomic(&manifest_path, &manifest)?;

        self.tables.insert(
            name.to_string(),
            TableEntry {
                id,
                name: name.to_string(),
                dir: dir_rel,
            },
        );
        self.next_table_id += 1;

        tracing::debug!(table = %name, id, "publishing table in catalog");
        if let Err(e) = self.save_atomic() {
            // Rollback: undo in-memory changes and remove the orphan dir we
            // just created. Failure to clean up the dir is logged but does
            // not mask the original error.
            tracing::debug!(table = %name, "rolling back create after catalog write failure");
            self.tables.remove(name);
            self.next_table_id -= 1;
            if let Err(cleanup_err) = fs::remove_dir_all(&dir_abs) {
                tracing::warn!(
                    path = %dir_abs.display(),
                    error = %cleanup_err,
                    "failed to clean up table dir after catalog write error"
                );
            }
            return Err(e);
        }

        tracing::info!(table = %name, id, "created table");
        Ok(id)
    }

    pub fn list_tables(&self) -> Vec<&str> {
        self.tables.keys().map(String::as_str).collect()
    }

    /// Absolute path to a table's directory. Used by storage-track code
    /// (column-store row groups, etc.) that needs to lay files out below
    /// the catalog-managed table dir without going through `describe_table`.
    pub fn table_dir(&self, name: &str) -> Result<PathBuf, Error> {
        let entry = self
            .tables
            .get(name)
            .ok_or_else(|| Error::no_such_table(name))?;
        Ok(self.root.join(&entry.dir))
    }

    /// Absolute path to a table's manifest file.
    fn manifest_path(&self, name: &str) -> Result<PathBuf, Error> {
        let entry = self
            .tables
            .get(name)
            .ok_or_else(|| Error::no_such_table(name))?;
        Ok(self.root.join(&entry.dir).join(MANIFEST_FILE))
    }

    /// Read and verify a table's manifest from disk.
    fn read_manifest(&self, name: &str) -> Result<ManifestFile, Error> {
        let path = self.manifest_path(name)?;
        tracing::debug!(table = %name, path = %path.display(), "reading manifest");
        let bytes = fs::read(&path).map_err(|e| Error::io("read manifest.toml", e))?;
        let body = checksum::verify(&bytes)
            .map_err(|e| Error::invalid_schema(format!("manifest.toml: {e}")))?;
        let content = std::str::from_utf8(body)
            .map_err(|_| Error::invalid_schema("manifest.toml is not valid UTF-8".to_string()))?;
        toml::from_str(content).map_err(|e| Error::invalid_schema(format!("manifest.toml: {e}")))
    }

    /// Serialize `manifest` and replace its file atomically: write a sibling
    /// temp file, fsync it, then rename it into place.
    fn write_manifest_atomic(path: &Path, manifest: &ManifestFile) -> Result<(), Error> {
        let serialized = toml::to_string(manifest)
            .map_err(|e| Error::invalid_schema(format!("serialize manifest: {e}")))?;
        let wrapped = checksum::wrap(serialized.as_bytes());
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, &wrapped).map_err(|e| Error::io("write manifest tmp", e))?;
        fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| Error::io("fsync manifest tmp", e))?;
        fs::rename(&tmp, path).map_err(|e| Error::io("rename manifest", e))?;
        Ok(())
    }

    pub fn describe_table(&self, name: &str) -> Result<TableDescriptor, Error> {
        let entry = self
            .tables
            .get(name)
            .ok_or_else(|| Error::no_such_table(name))?;
        let dir = self.root.join(&entry.dir);
        let manifest = self.read_manifest(name)?;
        let schema = Schema {
            columns: manifest
                .columns
                .iter()
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: c.ty,
                    nullable: c.nullable,
                })
                .collect(),
        };
        Ok(TableDescriptor {
            id: manifest.table_id,
            name: manifest.name,
            schema,
            row_group_size: manifest.row_group_size,
            storage_track: manifest.storage_track,
            dir,
            next_rid: manifest.next_rid,
        })
    }

    /// Persist a table's `next_rid` by rewriting its manifest atomically.
    /// Called by the insert path after a row's column data is durable.
    pub fn set_next_rid(&self, name: &str, next_rid: u64) -> Result<(), Error> {
        let mut manifest = self.read_manifest(name)?;
        manifest.next_rid = next_rid;
        let path = self.manifest_path(name)?;
        Self::write_manifest_atomic(&path, &manifest)
    }

    /// At the catalog level, opening is the same as describing; it may later
    /// grow to cache file handles or row-group indexes on the descriptor.
    pub fn open_table(&self, name: &str) -> Result<TableDescriptor, Error> {
        self.describe_table(name)
    }

    pub fn drop_table(&mut self, name: &str) -> Result<(), Error> {
        tracing::info!(table = %name, "dropping table");
        let entry = self
            .tables
            .remove(name)
            .ok_or_else(|| Error::no_such_table(name))?;
        let dir_abs = self.root.join(&entry.dir);

        // Catalog truth before fs cleanup: a crash between these two steps
        // leaves an orphan directory but a consistent catalog. Reverse order
        // would leave the catalog pointing at a missing table dir.
        tracing::debug!(table = %name, "removing from catalog");
        if let Err(e) = self.save_atomic() {
            tracing::debug!(table = %name, "restoring entry after catalog write failure");
            self.tables.insert(name.to_string(), entry);
            return Err(e);
        }
        tracing::debug!(path = %dir_abs.display(), "removing table dir");
        if let Err(e) = fs::remove_dir_all(&dir_abs) {
            tracing::warn!(
                path = %dir_abs.display(),
                error = %e,
                "failed to remove dropped table dir"
            );
        }
        tracing::info!(table = %name, "dropped table");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn schema_users() -> Schema {
        Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    ty: ColumnType::Int,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    ty: ColumnType::Text,
                    nullable: true,
                },
            ],
        }
    }

    #[test]
    fn load_on_empty_dir_returns_empty_catalog() {
        let tmp = TempDir::new().unwrap();
        let cat = Catalog::load(tmp.path()).unwrap();
        assert!(cat.list_tables().is_empty());
    }

    #[test]
    fn create_table_writes_catalog_layout() {
        // Catalog only owns the table dir + manifest. Storage-track layout
        // (row_groups/...) is materialized one level up in ColumnStore.
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        let id = cat
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        assert_eq!(id, 1);

        let table_dir = tmp.path().join("tables").join("00000001");
        assert!(table_dir.is_dir());
        assert!(table_dir.join("manifest.toml").is_file());
        assert!(tmp.path().join("catalog.toml").is_file());
        assert!(!tmp.path().join("catalog.toml.tmp").exists());
    }

    #[test]
    fn table_dir_returns_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let dir = cat.table_dir("users").unwrap();
        assert_eq!(dir, tmp.path().join("tables").join("00000001"));
    }

    #[test]
    fn table_dir_for_unknown_table_fails() {
        let tmp = TempDir::new().unwrap();
        let cat = Catalog::load(tmp.path()).unwrap();
        let err = cat.table_dir("ghosts").unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }

    #[test]
    fn create_table_persists_storage_track_in_catalog() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();

        let raw = fs::read_to_string(tmp.path().join("catalog.toml")).unwrap();
        assert!(raw.contains("storage_track = \"column-store\""));
    }

    #[test]
    fn create_table_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let err = cat
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn create_table_rejects_invalid_schema() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        let bad = Schema { columns: vec![] };
        let err = cat
            .create_table("users", bad, TableOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("at least one column"));
    }

    #[test]
    fn list_tables_returns_all_created_names() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("orders", schema_users(), TableOptions::default())
            .unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        cat.create_table("audit", schema_users(), TableOptions::default())
            .unwrap();
        let names: HashSet<&str> = cat.list_tables().into_iter().collect();
        assert_eq!(names, HashSet::from(["audit", "orders", "users"]),);
    }

    #[test]
    fn describe_table_round_trips_schema() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();

        let desc = cat.describe_table("users").unwrap();
        assert_eq!(desc.name, "users");
        assert_eq!(desc.id, 1);
        assert_eq!(desc.row_group_size, DEFAULT_ROW_GROUP_SIZE);
        assert_eq!(desc.storage_track, "column-store");
        assert_eq!(desc.next_rid, 0);
        assert_eq!(desc.schema, schema_users());
    }

    #[test]
    fn describe_table_for_unknown_table_fails() {
        let tmp = TempDir::new().unwrap();
        let cat = Catalog::load(tmp.path()).unwrap();
        let err = cat.describe_table("ghosts").unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }

    #[test]
    fn next_table_id_survives_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let mut cat = Catalog::load(tmp.path()).unwrap();
            cat.create_table("a", schema_users(), TableOptions::default())
                .unwrap();
            cat.create_table("b", schema_users(), TableOptions::default())
                .unwrap();
        }
        let mut cat = Catalog::load(tmp.path()).unwrap();
        let id = cat
            .create_table("c", schema_users(), TableOptions::default())
            .unwrap();
        assert_eq!(id, 3);
    }

    #[test]
    fn schema_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let mut cat = Catalog::load(tmp.path()).unwrap();
            cat.create_table("users", schema_users(), TableOptions::default())
                .unwrap();
        }
        let cat = Catalog::load(tmp.path()).unwrap();
        let desc = cat.describe_table("users").unwrap();
        assert_eq!(desc.schema, schema_users());
        assert_eq!(desc.id, 1);
    }

    #[test]
    fn drop_table_removes_entry_and_dir() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let dir = tmp.path().join("tables").join("00000001");
        assert!(dir.exists());

        cat.drop_table("users").unwrap();
        assert!(cat.list_tables().is_empty());
        assert!(!dir.exists());

        let err = cat.describe_table("users").unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }

    #[test]
    fn drop_table_on_unknown_table_fails() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        let err = cat.drop_table("ghosts").unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }

    #[test]
    fn drop_table_does_not_recycle_id() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("a", schema_users(), TableOptions::default())
            .unwrap();
        cat.drop_table("a").unwrap();
        let id = cat
            .create_table("b", schema_users(), TableOptions::default())
            .unwrap();
        assert_eq!(id, 2, "table_id must be monotonic across drops");
    }

    #[test]
    fn row_group_size_override_is_persisted() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table(
            "users",
            schema_users(),
            TableOptions {
                row_group_size: Some(4096),
            },
        )
        .unwrap();
        let desc = cat.describe_table("users").unwrap();
        assert_eq!(desc.row_group_size, 4096);
    }

    #[test]
    fn load_rejects_wrong_storage_track() {
        let tmp = TempDir::new().unwrap();
        // Wrap with a valid checksum so we exercise the storage_track guard,
        // not the integrity guard.
        let body = b"format_version = 1\nstorage_track = \"row-store\"\nnext_table_id = 1\n";
        fs::write(tmp.path().join("catalog.toml"), checksum::wrap(body)).unwrap();
        let err = Catalog::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("storage_track"));
    }

    #[test]
    fn load_detects_catalog_bit_flip() {
        let tmp = TempDir::new().unwrap();
        {
            let mut cat = Catalog::load(tmp.path()).unwrap();
            cat.create_table("users", schema_users(), TableOptions::default())
                .unwrap();
        }
        // Flip a byte in the body. The leading checksum line stays put;
        // verify() recomputes over the body and notices.
        let path = tmp.path().join("catalog.toml");
        let mut bytes = fs::read(&path).unwrap();
        let target = bytes.len() - 5;
        bytes[target] ^= 0x01;
        fs::write(&path, &bytes).unwrap();

        let err = Catalog::load(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("checksum"),
            "expected checksum error, got: {err}"
        );
    }

    #[test]
    fn describe_table_detects_manifest_bit_flip() {
        let tmp = TempDir::new().unwrap();
        let mut cat = Catalog::load(tmp.path()).unwrap();
        cat.create_table("users", schema_users(), TableOptions::default())
            .unwrap();

        let path = tmp
            .path()
            .join("tables")
            .join("00000001")
            .join("manifest.toml");
        let mut bytes = fs::read(&path).unwrap();
        let target = bytes.len() - 5;
        bytes[target] ^= 0x01;
        fs::write(&path, &bytes).unwrap();

        let err = cat.describe_table("users").unwrap_err();
        assert!(
            err.to_string().contains("checksum"),
            "expected checksum error, got: {err}"
        );
    }

    #[test]
    fn set_next_rid_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let mut cat = Catalog::load(tmp.path()).unwrap();
            cat.create_table("users", schema_users(), TableOptions::default())
                .unwrap();
            cat.set_next_rid("users", 5).unwrap();
            assert_eq!(cat.describe_table("users").unwrap().next_rid, 5);
        }
        let cat = Catalog::load(tmp.path()).unwrap();
        assert_eq!(cat.describe_table("users").unwrap().next_rid, 5);
    }

    #[test]
    fn set_next_rid_on_unknown_table_fails() {
        let tmp = TempDir::new().unwrap();
        let cat = Catalog::load(tmp.path()).unwrap();
        let err = cat.set_next_rid("ghosts", 1).unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }
}
