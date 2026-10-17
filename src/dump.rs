//! PostgreSQL client tools: "Export with 'pg_dump'",
//! "Restore with 'pg_restore'" and "Restore with 'psql'". Finds the tools,
//! turns dialog options into arguments and the data source into libpq
//! environment variables, so the password never appears on a command line.

use crate::model::{DataSource, TlsMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    PgDump,
    PgRestore,
    Psql,
}

impl Tool {
    pub fn binary(self) -> &'static str {
        match self {
            Tool::PgDump => "pg_dump",
            Tool::PgRestore => "pg_restore",
            Tool::Psql => "psql",
        }
    }
}

/// Paths chosen in the dialogs; empty means "find it".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolPaths {
    pub pg_dump: String,
    pub pg_restore: String,
    pub psql: String,
}

impl ToolPaths {
    pub fn get(&self, tool: Tool) -> &str {
        match tool {
            Tool::PgDump => &self.pg_dump,
            Tool::PgRestore => &self.pg_restore,
            Tool::Psql => &self.psql,
        }
    }

    pub fn get_mut(&mut self, tool: Tool) -> &mut String {
        match tool {
            Tool::PgDump => &mut self.pg_dump,
            Tool::PgRestore => &mut self.pg_restore,
            Tool::Psql => &mut self.psql,
        }
    }
}

/// Folders where PostgreSQL installs its client tools, after `PATH`.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let fixed = [
        "/opt/homebrew/bin",
        "/opt/homebrew/opt/libpq/bin",
        "/usr/local/bin",
        "/usr/local/opt/libpq/bin",
        "/Applications/Postgres.app/Contents/Versions/latest/bin",
        "/usr/bin",
    ];
    dirs.extend(fixed.iter().map(PathBuf::from));
    // Debian and Ubuntu keep one folder per major version; prefer the newest.
    dirs.extend(version_dirs(PathBuf::from("/usr/lib/postgresql")));
    // The Windows installer uses C:\Program Files\PostgreSQL\<major>\bin.
    #[cfg(windows)]
    for root in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(root) {
            dirs.extend(version_dirs(PathBuf::from(root).join("PostgreSQL")));
        }
    }
    dirs
}

/// `<root>/<version>/bin` folders, newest major version first.
fn version_dirs(root: PathBuf) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut versions: Vec<(u32, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let major = e.file_name().to_string_lossy().split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
            (major, e.path().join("bin"))
        })
        .collect();
    versions.sort();
    versions.into_iter().rev().map(|(_, dir)| dir).collect()
}

/// The configured tool if it exists, otherwise the first one found.
pub fn find_tool(tool: Tool, configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }
    candidate_dirs()
        .into_iter()
        .map(|dir| dir.join(format!("{}{}", tool.binary(), std::env::consts::EXE_SUFFIX)))
        .find(|path| path.is_file())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DumpFormat {
    /// A SQL script, restored with psql.
    #[default]
    Plain,
    /// pg_dump's compressed archive, restored with pg_restore.
    Custom,
    /// One file per table in a folder; dumps and restores in parallel.
    Directory,
    Tar,
}

impl DumpFormat {
    pub const ALL: [DumpFormat; 4] = [DumpFormat::Plain, DumpFormat::Custom, DumpFormat::Directory, DumpFormat::Tar];

    pub fn label(self) -> &'static str {
        match self {
            DumpFormat::Plain => "Plain SQL script",
            DumpFormat::Custom => "Custom archive",
            DumpFormat::Directory => "Directory",
            DumpFormat::Tar => "Tar archive",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            DumpFormat::Plain => "p",
            DumpFormat::Custom => "c",
            DumpFormat::Directory => "d",
            DumpFormat::Tar => "t",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            DumpFormat::Plain => "sql",
            DumpFormat::Custom => "dump",
            DumpFormat::Directory => "",
            DumpFormat::Tar => "tar",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    #[default]
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

impl Content {
    pub const ALL: [Content; 3] = [Content::SchemaAndData, Content::SchemaOnly, Content::DataOnly];

    pub fn label(self) -> &'static str {
        match self {
            Content::SchemaAndData => "Schema and data",
            Content::SchemaOnly => "Schema only",
            Content::DataOnly => "Data only",
        }
    }

    fn flag(self) -> Option<&'static str> {
        match self {
            Content::SchemaAndData => None,
            Content::SchemaOnly => Some("--schema-only"),
            Content::DataOnly => Some("--data-only"),
        }
    }
}

/// How table rows are written to a plain dump.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Statements {
    #[default]
    Copy,
    Inserts,
    ColumnInserts,
}

impl Statements {
    pub const ALL: [Statements; 3] = [Statements::Copy, Statements::Inserts, Statements::ColumnInserts];

    pub fn label(self) -> &'static str {
        match self {
            Statements::Copy => "COPY",
            Statements::Inserts => "INSERT",
            Statements::ColumnInserts => "INSERT with columns",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DumpOptions {
    pub format: DumpFormat,
    pub content: Content,
    pub statements: Statements,
    /// Rows per INSERT statement (`--rows-per-insert`); 1 is pg_dump's default.
    pub rows_per_insert: u32,
    /// `DROP` objects before creating them.
    pub clean: bool,
    pub if_exists: bool,
    /// Include `CREATE DATABASE`.
    pub create: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
    /// Parallel jobs; the directory format only.
    pub jobs: u32,
    /// 0 to 9; `None` keeps pg_dump's default.
    pub compression: Option<u8>,
    /// Extra arguments, split like a shell would.
    pub extra: String,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            format: DumpFormat::Plain,
            content: Content::SchemaAndData,
            statements: Statements::Copy,
            rows_per_insert: 1,
            clean: false,
            if_exists: false,
            create: false,
            no_owner: false,
            no_privileges: false,
            jobs: 1,
            compression: None,
            extra: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoreOptions {
    pub content: Content,
    pub clean: bool,
    pub if_exists: bool,
    /// Create the database named in the archive and restore into it.
    pub create: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
    /// All or nothing (`--single-transaction` / `psql -1`).
    pub single_transaction: bool,
    /// Stop at the first error (`--exit-on-error` / `ON_ERROR_STOP`).
    pub exit_on_error: bool,
    pub jobs: u32,
    pub extra: String,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            content: Content::SchemaAndData,
            clean: false,
            if_exists: false,
            create: false,
            no_owner: false,
            no_privileges: false,
            single_transaction: false,
            exit_on_error: true,
            jobs: 1,
            extra: String::new(),
        }
    }
}

/// Where the tool connects: the server itself or the local end of a tunnel.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
}

impl Endpoint {
    pub fn of(ds: &DataSource) -> Self {
        Self {
            host: ds.host.clone(),
            port: ds.port,
            user: ds.user.clone(),
            database: if ds.database.is_empty() { crate::drivers::DEFAULT_DATABASE.to_string() } else { ds.database.clone() },
        }
    }

    fn args(&self, out: &mut Vec<String>) {
        out.extend([
            "--host".into(),
            self.host.clone(),
            "--port".into(),
            self.port.to_string(),
            "--username".into(),
            self.user.clone(),
            // Never prompt: a missing password fails instead of hanging the task.
            "--no-password".into(),
        ]);
    }
}

/// What pg_dump writes and which objects it includes.
#[derive(Clone, Debug, PartialEq)]
pub struct DumpTarget {
    pub output: PathBuf,
    pub schemas: Vec<String>,
    /// Qualified as `schema.table`.
    pub tables: Vec<String>,
}

pub fn dump_args(options: &DumpOptions, target: &DumpTarget, endpoint: &Endpoint) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    endpoint.args(&mut args);
    args.extend(["--format".into(), options.format.flag().into(), "--verbose".into()]);
    if let Some(flag) = options.content.flag() {
        args.push(flag.into());
    }
    match options.statements {
        Statements::Copy => {}
        Statements::Inserts => args.push("--inserts".into()),
        Statements::ColumnInserts => args.push("--column-inserts".into()),
    }
    if options.statements != Statements::Copy && options.rows_per_insert > 1 {
        args.push(format!("--rows-per-insert={}", options.rows_per_insert));
    }
    if options.clean {
        args.push("--clean".into());
        if options.if_exists {
            args.push("--if-exists".into());
        }
    }
    if options.create {
        args.push("--create".into());
    }
    if options.no_owner {
        args.push("--no-owner".into());
    }
    if options.no_privileges {
        args.push("--no-privileges".into());
    }
    if options.format == DumpFormat::Directory && options.jobs > 1 {
        args.push(format!("--jobs={}", options.jobs));
    }
    if let Some(level) = options.compression.filter(|_| options.format != DumpFormat::Tar) {
        args.push(format!("--compress={level}"));
    }
    for schema in &target.schemas {
        args.push(format!("--schema={}", object_pattern(schema)));
    }
    for table in &target.tables {
        args.push(format!("--table={}", table_pattern(table)));
    }
    args.extend(split_args(&options.extra)?);
    args.push(format!("--file={}", target.output.display()));
    args.push(endpoint.database.clone());
    Ok(args)
}

pub fn restore_args(options: &RestoreOptions, input: &Path, endpoint: &Endpoint) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    endpoint.args(&mut args);
    args.push("--verbose".into());
    if let Some(flag) = options.content.flag() {
        args.push(flag.into());
    }
    if options.clean {
        args.push("--clean".into());
        if options.if_exists {
            args.push("--if-exists".into());
        }
    }
    if options.create {
        args.push("--create".into());
    }
    if options.no_owner {
        args.push("--no-owner".into());
    }
    if options.no_privileges {
        args.push("--no-privileges".into());
    }
    if options.single_transaction {
        args.push("--single-transaction".into());
    } else if options.jobs > 1 {
        args.push(format!("--jobs={}", options.jobs));
    }
    if options.exit_on_error {
        args.push("--exit-on-error".into());
    }
    args.extend(split_args(&options.extra)?);
    // With --create pg_restore connects to this database only to issue CREATE DATABASE.
    args.push(format!("--dbname={}", endpoint.database));
    args.push(input.display().to_string());
    Ok(args)
}

pub fn psql_args(options: &RestoreOptions, input: &Path, endpoint: &Endpoint) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    endpoint.args(&mut args);
    // Quiet drops the command tag of every statement; failing statements and
    // their errors still reach stderr.
    args.extend(["--quiet".into(), "--echo-errors".into(), "--no-psqlrc".into()]);
    if options.single_transaction {
        args.push("--single-transaction".into());
    }
    if options.exit_on_error {
        args.extend(["--set".into(), "ON_ERROR_STOP=1".into()]);
    }
    args.extend(split_args(&options.extra)?);
    args.push(format!("--file={}", input.display()));
    args.push(format!("--dbname={}", endpoint.database));
    Ok(args)
}

/// libpq environment for the data source: password and SSL settings.
pub fn environment(ds: &DataSource) -> Result<Vec<(String, String)>, String> {
    let mut env = vec![("PGAPPNAME".to_string(), "DBeast".to_string())];
    if !ds.password.is_empty() {
        env.push(("PGPASSWORD".into(), ds.password.clone()));
    }
    if !ds.use_tls {
        env.push(("PGSSLMODE".into(), "disable".into()));
        return Ok(env);
    }
    let mode = match ds.tls.mode {
        TlsMode::Require => "require",
        TlsMode::VerifyCa => "verify-ca",
        TlsMode::VerifyFull => "verify-full",
    };
    env.push(("PGSSLMODE".into(), mode.into()));
    if !ds.tls.ca_file.trim().is_empty() {
        env.push(("PGSSLROOTCERT".into(), ds.tls.ca_file.trim().into()));
    } else if ds.tls.mode != TlsMode::Require {
        // libpq's default is ~/.postgresql/root.crt; DBeast verifies against
        // the system trust store, which libpq reads with "system".
        env.push(("PGSSLROOTCERT".into(), "system".into()));
    }
    if !ds.tls.identity_file.trim().is_empty() {
        if ds.tls.key_file.trim().is_empty() {
            return Err("The PostgreSQL tools need the client certificate and its key as PEM files; a PKCS#12 archive cannot be passed to them.".into());
        }
        env.push(("PGSSLCERT".into(), ds.tls.identity_file.trim().into()));
        env.push(("PGSSLKEY".into(), ds.tls.key_file.trim().into()));
    }
    Ok(env)
}

/// pg_dump pattern for one exact name: double quotes keep case and dots literal.
fn object_pattern(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn table_pattern(qualified: &str) -> String {
    match qualified.split_once('.') {
        Some((schema, table)) => format!("{}.{}", object_pattern(schema), object_pattern(table)),
        None => object_pattern(qualified),
    }
}

/// Splits extra arguments like a POSIX shell: whitespace separates, single
/// and double quotes group, a backslash escapes the next character.
pub fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err("Unclosed ' in additional arguments".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => current.push(chars.next().ok_or("Unfinished escape in additional arguments")?),
                        Some(c) => current.push(c),
                        None => return Err("Unclosed \" in additional arguments".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                current.push(chars.next().ok_or("Unfinished escape in additional arguments")?);
            }
            c if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                started = true;
                current.push(c);
            }
        }
    }
    if started {
        args.push(current);
    }
    Ok(args)
}

/// The command as a shell line, for display and copying.
pub fn command_line(program: &Path, args: &[String]) -> String {
    std::iter::once(program.display().to_string())
        .chain(args.iter().cloned())
        .map(|arg| shell_quote(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:,@%+".contains(c)) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DriverKind;

    fn endpoint() -> Endpoint {
        Endpoint { host: "db.example".into(), port: 5432, user: "app".into(), database: "shop".into() }
    }

    #[test]
    fn dump_arguments_follow_the_options() {
        let options = DumpOptions {
            format: DumpFormat::Directory,
            content: Content::DataOnly,
            statements: Statements::ColumnInserts,
            rows_per_insert: 100,
            clean: true,
            if_exists: true,
            no_owner: true,
            jobs: 4,
            compression: Some(6),
            extra: "--exclude-table-data='audit.*' --lock-wait-timeout=5s".into(),
            ..DumpOptions::default()
        };
        let target = DumpTarget {
            output: "/tmp/out".into(),
            schemas: vec!["Sales".into()],
            tables: vec!["public.order.items".into()],
        };
        let args = dump_args(&options, &target, &endpoint()).unwrap();
        assert_eq!(
            args,
            [
                "--host", "db.example", "--port", "5432", "--username", "app", "--no-password",
                "--format", "d", "--verbose", "--data-only", "--column-inserts", "--rows-per-insert=100",
                "--clean", "--if-exists", "--no-owner", "--jobs=4", "--compress=6",
                "--schema=\"Sales\"", "--table=\"public\".\"order.items\"",
                "--exclude-table-data=audit.*", "--lock-wait-timeout=5s",
                "--file=/tmp/out", "shop",
            ]
        );
    }

    #[test]
    fn restore_arguments_for_pg_restore_and_psql() {
        let options = RestoreOptions { clean: true, if_exists: true, single_transaction: true, jobs: 4, ..RestoreOptions::default() };
        let args = restore_args(&options, Path::new("/tmp/shop.dump"), &endpoint()).unwrap();
        assert!(args.contains(&"--single-transaction".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--jobs")), "parallel jobs cannot run in one transaction");
        assert!(args.ends_with(&["--dbname=shop".into(), "/tmp/shop.dump".into()]));
        let psql = psql_args(&options, Path::new("/tmp/shop.sql"), &endpoint()).unwrap();
        assert!(psql.windows(2).any(|w| w == ["--set", "ON_ERROR_STOP=1"]));
        assert!(psql.contains(&"--file=/tmp/shop.sql".to_string()));
        assert!(psql.contains(&"--quiet".to_string()) && psql.contains(&"--echo-errors".to_string()), "errors stay visible without command tags");
    }

    #[test]
    fn environment_carries_password_and_tls_not_arguments() {
        let mut ds = DataSource::new(DriverKind::Postgres);
        ds.password = "s3cret".into();
        let env = environment(&ds).unwrap();
        assert!(env.contains(&("PGPASSWORD".into(), "s3cret".into())));
        assert!(env.contains(&("PGSSLMODE".into(), "disable".into())));
        let args = dump_args(&DumpOptions::default(), &DumpTarget { output: "/tmp/x.sql".into(), schemas: vec![], tables: vec![] }, &Endpoint::of(&ds)).unwrap();
        assert!(!args.iter().any(|a| a.contains("s3cret")));
        ds.use_tls = true;
        ds.tls.mode = TlsMode::VerifyCa;
        ds.tls.ca_file = "/certs/ca.pem".into();
        ds.tls.identity_file = "/certs/client.p12".into();
        assert!(environment(&ds).unwrap_err().contains("PEM"));
        ds.tls.identity_file = "/certs/client.pem".into();
        ds.tls.key_file = "/certs/client.key".into();
        let env = environment(&ds).unwrap();
        assert!(env.contains(&("PGSSLMODE".into(), "verify-ca".into())));
        assert!(env.contains(&("PGSSLROOTCERT".into(), "/certs/ca.pem".into())));
        assert!(env.contains(&("PGSSLKEY".into(), "/certs/client.key".into())));
    }

    #[test]
    fn extra_arguments_split_like_a_shell() {
        assert_eq!(split_args(r#"-a "b c" 'd e' f\ g"#).unwrap(), ["-a", "b c", "d e", "f g"]);
        assert_eq!(split_args("  ").unwrap(), Vec::<String>::new());
        assert!(split_args("'open").is_err());
        assert_eq!(
            command_line(Path::new("/usr/bin/pg_dump"), &["--file=/tmp/a b.sql".into(), "shop".into()]),
            "/usr/bin/pg_dump '--file=/tmp/a b.sql' shop"
        );
    }
}
