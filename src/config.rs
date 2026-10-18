use crate::model::{DataSource, SchemaSelection};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedConsole {
    pub title: String,
    pub ds_id: String,
    /// Shared with the console tab, so a snapshot copies no text.
    pub sql: std::sync::Arc<str>,
    /// Database of the server the console runs in, when not the data source's default.
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub schema: SchemaSelection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub ds_id: String,
    pub sql: std::sync::Arc<str>,
    pub at: chrono::DateTime<chrono::Local>,
}

/// Executed statements kept per data source.
const HISTORY_LIMIT: usize = 300;

/// A WHERE or ORDER BY text applied in a table editor.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FilterEntry {
    pub ds_id: String,
    /// Qualified table name as shown in the editor.
    pub table: String,
    /// "WHERE" or "ORDER BY".
    pub clause: String,
    pub text: String,
}

/// Font sizes and grid look chosen in the Appearance dialog.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Appearance {
    /// SQL editor font size in points.
    pub editor_font_size: f32,
    /// Result grid font size in points.
    pub grid_font_size: f32,
    pub row_density: RowDensity,
    pub striped_rows: bool,
}

impl Appearance {
    pub const FONT_SIZES: std::ops::RangeInclusive<f32> = 9.0..=24.0;

    /// Values within the supported ranges, whatever a hand-edited file holds.
    pub fn clamped(mut self) -> Self {
        let clamp = |size: f32, default: f32| {
            if size.is_finite() {
                // Half points: every distinct size rasterizes its own glyphs.
                ((size * 2.0).round() / 2.0).clamp(*Self::FONT_SIZES.start(), *Self::FONT_SIZES.end())
            } else {
                default
            }
        };
        let defaults = Self::default();
        self.editor_font_size = clamp(self.editor_font_size, defaults.editor_font_size);
        self.grid_font_size = clamp(self.grid_font_size, defaults.grid_font_size);
        self
    }

    /// Height of a grid row: the font plus the padding of the chosen density.
    pub fn grid_row_height(&self) -> f32 {
        let padding = match self.row_density {
            RowDensity::Compact => 7.0,
            RowDensity::Default => 9.0,
            RowDensity::Comfortable => 13.0,
        };
        (self.grid_font_size + padding).round()
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            editor_font_size: 13.5,
            grid_font_size: 13.0,
            row_density: RowDensity::Default,
            striped_rows: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RowDensity {
    Compact,
    #[default]
    Default,
    Comfortable,
}

impl RowDensity {
    pub const ALL: [RowDensity; 3] = [Self::Compact, Self::Default, Self::Comfortable];

    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Default => "Default",
            Self::Comfortable => "Comfortable",
        }
    }
}

/// Filters kept per table and clause, as a filter history.
const FILTER_HISTORY_LIMIT: usize = 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub data_sources: Vec<DataSource>,
    pub credential_namespace: String,
    /// Absent in older per-source Keychain configurations.
    #[serde(default)]
    pub credential_vault_version: u8,
    /// A durable configuration commit precedes removal of old vault revisions.
    pub credential_vault_cleanup: bool,
    /// Old revisions awaiting deletion; retry after a denied Keychain prompt.
    pub credential_cleanup: Vec<String>,
    pub consoles: Vec<SavedConsole>,
    /// Data source groups in Database Explorer order. Nested groups use "/"
    /// ("Clients/Acme"). Kept outside DataSource so moving a
    /// source never changes its connection identity.
    pub source_groups: Vec<String>,
    /// Group of each data source by ID; absent means the top level.
    pub source_group: BTreeMap<String, String>,
    pub max_rows: usize,
    pub mcp_port: u16,
    /// Agent connections and sharing are stored in the Keychain (`mcp-v1`).
    /// Read it only when set, so unused MCP never triggers a Keychain prompt.
    pub mcp_state_saved: bool,
    /// Start the local MCP server when DBeast opens.
    pub mcp_autostart: bool,
    /// Scale of the whole interface (Zoom IDE), 1.0 is 100 %.
    pub ui_zoom: f32,
    /// The About DBeast music was muted; it stays muted on later openings.
    pub about_muted: bool,
    pub appearance: Appearance,
    pub history: Vec<HistoryEntry>,
    pub filter_history: Vec<FilterEntry>,
    /// Folder shown in the Files tool window.
    pub files_root: Option<PathBuf>,
    /// Base map tiles of the Geo Viewer, `{z}/{x}/{y}` template; empty disables them.
    pub tile_url: String,
    /// Extractor settings of the last data export.
    pub export_options: crate::export::ExportOptions,
    /// Folder of the last export, import or dump file.
    pub transfer_dir: Option<PathBuf>,
    /// Paths to pg_dump, pg_restore and psql; empty finds them.
    pub pg_tools: crate::dump::ToolPaths,
    pub dump_options: crate::dump::DumpOptions,
    pub restore_options: crate::dump::RestoreOptions,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_sources: Vec::new(),
            credential_namespace: uuid::Uuid::new_v4().to_string(),
            credential_vault_version: 1,
            credential_vault_cleanup: false,
            credential_cleanup: Vec::new(),
            consoles: Vec::new(),
            source_groups: Vec::new(),
            source_group: BTreeMap::new(),
            max_rows: 500,
            mcp_port: crate::mcp::DEFAULT_PORT,
            mcp_state_saved: false,
            mcp_autostart: false,
            ui_zoom: 1.0,
            about_muted: false,
            appearance: Appearance::default(),
            history: Vec::new(),
            filter_history: Vec::new(),
            files_root: None,
            tile_url: crate::ui::OSM_TILES.to_string(),
            export_options: Default::default(),
            transfer_dir: None,
            pg_tools: Default::default(),
            dump_options: Default::default(),
            restore_options: Default::default(),
        }
    }
}

impl Config {
    /// Records an applied filter at the top of its table's history.
    pub fn remember_filter(&mut self, ds_id: &str, table: &str, clause: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let same = |e: &FilterEntry| e.ds_id == ds_id && e.table == table && e.clause == clause;
        self.filter_history.retain(|e| !(same(e) && e.text == text));
        self.filter_history.push(FilterEntry {
            ds_id: ds_id.to_string(),
            table: table.to_string(),
            clause: clause.to_string(),
            text: text.to_string(),
        });
        let count = self.filter_history.iter().filter(|e| same(e)).count();
        if count > FILTER_HISTORY_LIMIT
            && let Some(oldest) = self.filter_history.iter().position(same)
        {
            self.filter_history.remove(oldest);
        }
    }

    /// Filters used on a table, most recent first.
    pub fn filters(&self, ds_id: &str, table: &str, clause: &str) -> Vec<String> {
        self.filter_history
            .iter()
            .rev()
            .filter(|e| e.ds_id == ds_id && e.table == table && e.clause == clause)
            .map(|e| e.text.clone())
            .collect()
    }

    /// Records a statement, moving a repeated one to the top instead of duplicating it.
    pub fn remember(&mut self, ds_id: &str, sql: &str) {
        self.history.retain(|h| !(h.ds_id == ds_id && &*h.sql == sql));
        self.history.push(HistoryEntry {
            ds_id: ds_id.to_string(),
            sql: sql.into(),
            at: chrono::Local::now(),
        });
        let count = self.history.iter().filter(|h| h.ds_id == ds_id).count();
        if count > HISTORY_LIMIT
            && let Some(oldest) = self.history.iter().position(|h| h.ds_id == ds_id)
        {
            self.history.remove(oldest);
        }
    }
}

impl Config {
    pub(crate) fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("DBEAST_CONFIG") {
            return Some(PathBuf::from(p));
        }
        // Saved passwords are bound to the config path, so an existing config from
        // before the rename stays where it is instead of being moved. The disk is
        // checked once per run: this path is asked for on every session save.
        static DEFAULT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                let legacy = directories::ProjectDirs::from("dev", "db_eye", "db_eye")
                    .map(|dirs| dirs.config_dir().join("config.json"))
                    .filter(|path| path.exists());
                legacy.or_else(|| {
                    directories::ProjectDirs::from("dev", "dbeast", "dbeast")
                        .map(|dirs| dirs.config_dir().join("config.json"))
                })
            })
            .clone()
    }

    /// Directory with the config file; also holds the SSH known_hosts.
    pub fn data_dir() -> PathBuf {
        Self::path()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir)
    }
}

/// Renames `temp` over `path`; `Ok(false)` means the rename happened but its
/// durability is not confirmed, so callers keep both credential revisions
/// until a later durable save succeeds.
#[cfg(not(windows))]
fn replace_durably(temp: &std::path::Path, path: &std::path::Path, parent: &std::path::Path) -> Result<bool, String> {
    std::fs::rename(temp, path).map_err(|_| "Cannot replace configuration".to_string())?;
    Ok(std::fs::File::open(parent).and_then(|dir| dir.sync_all()).is_ok())
}

/// A directory cannot be opened and flushed on Windows; MOVEFILE_WRITE_THROUGH
/// returns only after the rename reached the disk.
#[cfg(windows)]
fn replace_durably(temp: &std::path::Path, path: &std::path::Path, _: &std::path::Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let wide = |p: &std::path::Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    let (from, to) = (wide(temp), wide(path));
    // SAFETY: both arguments are NUL-terminated UTF-16 paths that outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err("Cannot replace configuration".to_string());
    }
    Ok(true)
}

/// Write with restrictive permissions before the first byte; never truncate the old file.
pub(crate) fn atomic_write<T: Serialize>(
    path: &std::path::Path,
    config: &T,
) -> Result<bool, String> {
    let data =
        serde_json::to_vec_pretty(config).map_err(|_| "Cannot encode configuration".to_string())?;
    atomic_write_bytes(path, &data)
}

/// [`atomic_write`] of an already encoded file.
pub(crate) fn atomic_write_bytes(path: &std::path::Path, data: &[u8]) -> Result<bool, String> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|_| "Cannot create configuration directory".to_string())?;
    let temp = parent.join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|_| "Cannot create temporary configuration".to_string())?;
        file.write_all(data)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot write configuration".to_string())?;
        replace_durably(&temp, path, parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Config, RowDensity};

    #[test]
    fn appearance_defaults_ranges_and_row_heights() {
        let old: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(old.appearance, Appearance::default(), "older files get the defaults");
        let wild = Appearance {
            editor_font_size: f32::NAN,
            grid_font_size: 80.0,
            ..Appearance::default()
        }
        .clamped();
        assert_eq!((wild.editor_font_size, wild.grid_font_size), (13.5, 24.0));
        let fine = Appearance { editor_font_size: 13.3, grid_font_size: 12.8, ..Appearance::default() }.clamped();
        assert_eq!((fine.editor_font_size, fine.grid_font_size), (13.5, 13.0), "half-point steps");
        let heights: Vec<f32> = RowDensity::ALL
            .into_iter()
            .map(|row_density| Appearance { row_density, ..Appearance::default() }.grid_row_height())
            .collect();
        assert_eq!(heights, [20.0, 22.0, 26.0], "Default keeps the former 22 px rows");
    }

    #[test]
    fn filter_history_is_per_table_recent_first_and_capped() {
        let mut c = Config::default();
        c.remember_filter("ds", "t", "WHERE", "id = 1");
        c.remember_filter("ds", "t", "WHERE", " id > 5 ");
        c.remember_filter("ds", "t", "WHERE", "id = 1");
        c.remember_filter("ds", "t", "ORDER BY", "id desc");
        c.remember_filter("ds", "other", "WHERE", "x");
        c.remember_filter("ds", "t", "WHERE", "   ");
        assert_eq!(c.filters("ds", "t", "WHERE"), ["id = 1", "id > 5"]);
        assert_eq!(c.filters("ds", "t", "ORDER BY"), ["id desc"]);
        for i in 0..30 {
            c.remember_filter("ds", "t", "WHERE", &format!("id = {i}"));
        }
        let all = c.filters("ds", "t", "WHERE");
        assert_eq!(all.len(), 20);
        assert_eq!(all[0], "id = 29");
    }
}
