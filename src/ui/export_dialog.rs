//! Export Data dialog: extractor and its settings on the left, a live preview
//! of the output on the right. Exports a result, a table, or
//! the chosen tables of a schema (one file each) as a background transfer.

use super::{App, theme, transfers::TransferKind};
use crate::backend::{ExportItem, ExportJob, PREVIEW_ROWS};
use crate::export::{ExportFormat, ExportOptions, LineEnding, Quote, TableName, Writer};
use crate::model::{BoundQuery, CapturedRelatedQuery, DataSource, DbObject, DriverKind, ObjectKind, QueryResult, ResolvedContext};
use egui::{Context, Id, RichText, Ui};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const LABEL_WIDTH: f32 = 112.0;

pub(super) enum ExportSource {
    /// A console or table result: its query runs again without a row limit.
    Result {
        console_id: u64,
        ds: DataSource,
        sql: String,
        context: Option<ResolvedContext>,
        related: Option<Arc<CapturedRelatedQuery>>,
        title: String,
    },
    Table {
        ds: DataSource,
        schema: Option<String>,
        table: String,
    },
    /// Chosen tables of a schema, one file each into a folder.
    Schema {
        ds: DataSource,
        schema: Option<String>,
    },
}

enum Tables {
    Loading,
    Loaded(Vec<TableChoice>),
    Failed(String),
}

struct TableChoice {
    name: String,
    view: bool,
    chosen: bool,
}

pub(super) struct ExportDialog {
    source: ExportSource,
    options: ExportOptions,
    /// File for one item, folder for several.
    path: String,
    tables: Tables,
    tables_request: u64,
    preview: Option<Arc<QueryResult>>,
    preview_error: Option<String>,
    preview_request: u64,
    /// Table whose rows the preview shows (schema exports).
    preview_table: Option<String>,
    /// Rendered preview and the options it was rendered with.
    preview_text: Option<(ExportOptions, String)>,
}

impl ExportDialog {
    fn single_name(&self) -> Option<&str> {
        match &self.source {
            ExportSource::Result { title, .. } => Some(title),
            ExportSource::Table { table, .. } => Some(table),
            ExportSource::Schema { .. } => None,
        }
    }

    fn source_text(&self) -> String {
        match &self.source {
            ExportSource::Result { title, .. } => format!("Result: {title}"),
            ExportSource::Table { schema, table, .. } => match schema {
                Some(schema) => format!("{schema}.{table}"),
                None => table.clone(),
            },
            ExportSource::Schema { schema, ds } => match schema {
                Some(schema) => format!("Tables of {schema}"),
                None => format!("Tables of {}", ds.display_name()),
            },
        }
    }

    fn chosen_tables(&self) -> Vec<&TableChoice> {
        match &self.tables {
            Tables::Loaded(tables) => tables.iter().filter(|t| t.chosen).collect(),
            _ => Vec::new(),
        }
    }

    /// Reason the Export button is disabled.
    fn blocked(&self) -> Option<&'static str> {
        if self.path.trim().is_empty() {
            return Some("Choose where to save the data");
        }
        if matches!(self.source, ExportSource::Schema { .. }) && self.chosen_tables().is_empty() {
            return Some("Choose at least one table");
        }
        if self.options.format == ExportFormat::Csv && self.options.separator.is_empty() {
            return Some("Enter a separator");
        }
        None
    }

    fn preview_text(&mut self) -> Option<&str> {
        let data = self.preview.as_ref()?;
        let stale = self.preview_text.as_ref().is_none_or(|(options, _)| *options != self.options);
        if stale {
            let driver = match &self.source {
                ExportSource::Result { ds, .. } | ExportSource::Table { ds, .. } | ExportSource::Schema { ds, .. } => ds.driver,
            };
            let table = TableName {
                schema: match &self.source {
                    ExportSource::Table { schema, .. } | ExportSource::Schema { schema, .. } => schema.clone(),
                    ExportSource::Result { .. } => None,
                },
                table: self
                    .preview_table
                    .clone()
                    .or_else(|| self.single_name().map(str::to_string))
                    .unwrap_or_default(),
            };
            let mut writer = Writer::new(Vec::new(), self.options.clone(), driver, table);
            // Writing to memory cannot fail.
            writer.begin(&data.columns).expect("in-memory write");
            for row in data.rows.iter().take(PREVIEW_ROWS) {
                writer.row(row).expect("in-memory write");
            }
            let text = String::from_utf8_lossy(&writer.finish().expect("in-memory write")).into_owned();
            self.preview_text = Some((self.options.clone(), text));
        }
        self.preview_text.as_ref().map(|(_, text)| text.as_str())
    }

    /// Keeps the file extension in step with the chosen format.
    fn set_format(&mut self, format: ExportFormat) {
        let old = self.options.format;
        self.options.format = format;
        if self.single_name().is_some() {
            let path = PathBuf::from(&self.path);
            if path.extension().and_then(|e| e.to_str()) == Some(old.extension()) {
                self.path = path.with_extension(format.extension()).to_string_lossy().into_owned();
            }
        }
    }
}

/// Folder where export, import and dump dialogs start.
pub(super) fn transfer_dir(saved: &Option<PathBuf>) -> PathBuf {
    saved
        .clone()
        .filter(|dir| dir.is_dir())
        .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
        .unwrap_or_else(std::env::temp_dir)
}

/// File name without characters the file system rejects.
pub(super) fn file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    let stem = stem.trim();
    if stem.is_empty() { "export".into() } else { stem.chars().take(120).collect() }
}

/// Entries of the Import/Export submenu in Database Explorer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TransferEntry {
    ExportData,
    ImportData,
    Dump,
    Restore,
    RestorePsql,
}

/// Where an Explorer node points: the data source (with its database for
/// PostgreSQL), the schema and the table, as far as the node goes.
pub(super) struct NodeScope {
    pub ds: DataSource,
    pub schema: Option<String>,
    pub table: Option<String>,
}

pub(super) fn node_scope(ds: &DataSource, node: Option<&DbObject>) -> NodeScope {
    let path: &[String] = node.map_or(&[], |n| &n.path);
    let kind = node.map(|n| n.kind);
    let is_table = matches!(kind, Some(ObjectKind::Table | ObjectKind::View));
    match ds.driver {
        DriverKind::Postgres => {
            let database = path.first().cloned();
            NodeScope {
                ds: database.as_deref().map_or_else(|| ds.clone(), |db| ds.with_database(db)),
                schema: path.get(1).cloned(),
                table: if is_table { path.last().cloned() } else { None },
            }
        }
        DriverKind::MySql => NodeScope {
            ds: ds.clone(),
            schema: path.first().cloned(),
            table: if is_table { path.last().cloned() } else { None },
        },
        DriverKind::Sqlite => NodeScope {
            ds: ds.clone(),
            // The main database stays implicit, as in the table editor.
            schema: path.first().filter(|s| *s != "main" && path.len() > 1).cloned(),
            table: if is_table { path.last().cloned() } else { None },
        },
    }
}

/// Import/Export submenu with the entries for the node kind.
pub(super) fn transfer_menu(ui: &mut Ui, source: &DataSource, node: Option<&DbObject>, actions: &mut Vec<super::TreeAction>) {
    let kind = node.map(|n| n.kind);
    let table = matches!(kind, Some(ObjectKind::Table | ObjectKind::View));
    let container = matches!(kind, None | Some(ObjectKind::Database | ObjectKind::Schema));
    if !table && !container {
        return;
    }
    let mut push = |entry| actions.push(super::TreeAction::Transfer(source.id.clone(), node.cloned(), entry));
    ui.menu_button("Import/Export", |ui| {
        if ui.button(if table { "Export Data to File…" } else { "Export Data to Files…" }).clicked() {
            push(TransferEntry::ExportData);
            ui.close();
        }
        if kind != Some(ObjectKind::View) && ui.button("Import Data from File…").clicked() {
            push(TransferEntry::ImportData);
            ui.close();
        }
        if source.driver == DriverKind::Postgres {
            ui.separator();
            if ui.button("Export with 'pg_dump'…").clicked() {
                push(TransferEntry::Dump);
                ui.close();
            }
            if container {
                if ui.button("Restore with 'pg_restore'…").clicked() {
                    push(TransferEntry::Restore);
                    ui.close();
                }
                if ui.button("Restore with 'psql'…").clicked() {
                    push(TransferEntry::RestorePsql);
                    ui.close();
                }
            }
        }
    });
}

impl App {
    pub(super) fn open_transfer(&mut self, ds_id: &str, node: Option<&DbObject>, entry: TransferEntry) {
        let Some(ds) = self.source(ds_id).cloned() else {
            return;
        };
        let scope = node_scope(&ds, node);
        match entry {
            TransferEntry::ImportData => self.open_import_dialog(scope),
            TransferEntry::Dump => self.open_dump_dialog(scope, node.map(|n| n.kind)),
            TransferEntry::Restore => self.open_restore_dialog(scope, false),
            TransferEntry::RestorePsql => self.open_restore_dialog(scope, true),
            TransferEntry::ExportData => {
                let source = match scope.table {
                    Some(table) => ExportSource::Table { ds: scope.ds, schema: scope.schema, table },
                    None => ExportSource::Schema { ds: scope.ds, schema: scope.schema },
                };
                self.open_export_dialog(source, None);
            }
        }
    }

    pub(super) fn open_export_dialog(&mut self, source: ExportSource, preview: Option<Arc<QueryResult>>) {
        let options = self.config.export_options.clone();
        let dir = transfer_dir(&self.config.transfer_dir);
        let path = match &source {
            ExportSource::Result { title, .. } | ExportSource::Table { table: title, .. } => dir
                .join(format!("{}.{}", file_stem(title), options.format.extension()))
                .to_string_lossy()
                .into_owned(),
            ExportSource::Schema { .. } => dir.to_string_lossy().into_owned(),
        };
        self.transfer_request += 1;
        let request = self.transfer_request;
        let mut dialog = ExportDialog {
            source,
            options,
            path,
            tables: Tables::Loading,
            tables_request: 0,
            preview_error: None,
            preview_request: 0,
            preview_table: None,
            preview: preview.clone(),
            preview_text: None,
        };
        match &dialog.source {
            ExportSource::Schema { ds, schema } => {
                dialog.tables_request = request;
                self.backend.transfer_tables(request, ds.clone(), schema.clone());
            }
            ExportSource::Table { ds, schema, table } if preview.is_none() => {
                dialog.preview_request = request;
                let sql = crate::drivers::select_all(ds.driver, schema.as_deref(), table);
                self.backend.transfer_preview(request, ds.clone(), BoundQuery { sql, params: Vec::new() }, None);
            }
            _ => {}
        }
        self.export_dialog = Some(dialog);
    }

    pub(super) fn export_preview_loaded(&mut self, request: u64, result: Result<QueryResult, String>) -> bool {
        let Some(dialog) = self.export_dialog.as_mut().filter(|d| d.preview_request == request) else {
            return false;
        };
        match result {
            Ok(data) => {
                dialog.preview = Some(Arc::new(data));
                dialog.preview_error = None;
            }
            Err(error) => dialog.preview_error = Some(error),
        }
        dialog.preview_text = None;
        true
    }

    pub(super) fn export_tables_loaded(&mut self, request: u64, result: Result<Vec<(String, bool)>, String>) -> bool {
        let Some(dialog) = self.export_dialog.as_mut().filter(|d| d.tables_request == request) else {
            return false;
        };
        dialog.tables = match result {
            Ok(tables) => Tables::Loaded(
                tables
                    .into_iter()
                    .map(|(name, view)| TableChoice { name, chosen: !view, view })
                    .collect(),
            ),
            Err(error) => Tables::Failed(error),
        };
        true
    }

    pub(super) fn export_dialog_window(&mut self, ctx: &Context) {
        let Some(dialog) = self.export_dialog.as_mut() else {
            return;
        };
        let (mut export, mut cancel) = (false, false);
        let response = theme::Dialog::new(Id::new("export_data_dialog"), "Export Data")
            .default_size([860.0, 540.0])
            .min_size([560.0, 380.0])
            .show(ctx, |ui| {
                egui::Panel::bottom("export_data_footer")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(egui::Frame::new())
                    .show(ui, |ui| {
                        let blocked = dialog.blocked();
                        theme::dialog_footer(
                            ui,
                            |ui| {
                                if let Some(reason) = blocked {
                                    ui.label(RichText::new(reason).color(theme::TEXT_MUTED));
                                } else if dialog.single_name().is_some() && Path::new(dialog.path.trim()).is_file() {
                                    ui.label(RichText::new("The file exists and will be replaced").color(theme::SEARCH_MARKER));
                                }
                            },
                            |ui| {
                                cancel = theme::dialog_button(ui, "Cancel").clicked();
                                ui.add_enabled_ui(blocked.is_none(), |ui| {
                                    export = theme::primary_button(ui, "Export").clicked();
                                });
                            },
                        );
                    });
                egui::Panel::left("export_data_settings")
                    .resizable(true)
                    .default_size(380.0)
                    .size_range(320.0..=560.0)
                    .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 12, top: 0, bottom: 0 }))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt("export_settings").show(ui, |ui| settings(ui, dialog));
                    });
                preview(ui, dialog);
            });
        if response.should_close() {
            cancel = true;
        }
        if cancel {
            self.export_dialog = None;
        } else if export {
            let dialog = self.export_dialog.take().expect("open dialog");
            self.start_export(dialog);
        }
    }

    fn start_export(&mut self, dialog: ExportDialog) {
        let path = PathBuf::from(dialog.path.trim());
        let options = dialog.options.clone();
        self.config.export_options = options.clone();
        self.config.transfer_dir = if matches!(dialog.source, ExportSource::Schema { .. }) {
            Some(path.clone())
        } else {
            path.parent().map(Path::to_path_buf)
        };
        self.save_config();
        let extension = options.format.extension();
        match dialog.source {
            ExportSource::Result { console_id, ds, sql, context, related, title } => {
                let table = TableName::new(title.clone());
                let task = self.begin_transfer(TransferKind::Export, format!("Export {title}"), Some(path.clone()), Some(console_id));
                match related {
                    Some(query) => self.backend.export_related(task, ds, query, options, table, path),
                    None => self.backend.export(task, ds, sql, options, table, path, context),
                }
            }
            ExportSource::Table { ds, schema, table } => {
                let task = self.begin_transfer(TransferKind::Export, format!("Export {table}"), Some(path.clone()), None);
                let sql = crate::drivers::select_all(ds.driver, schema.as_deref(), &table);
                self.backend.export_data(
                    task,
                    ExportJob {
                        items: vec![ExportItem {
                            ds,
                            query: BoundQuery { sql, params: Vec::new() },
                            context: None,
                            table: TableName { schema, table },
                            path,
                        }],
                        options,
                    },
                );
            }
            ExportSource::Schema { ds, schema } => {
                let Tables::Loaded(tables) = dialog.tables else {
                    return;
                };
                let items: Vec<ExportItem> = tables
                    .into_iter()
                    .filter(|t| t.chosen)
                    .map(|t| ExportItem {
                        ds: ds.clone(),
                        query: BoundQuery {
                            sql: crate::drivers::select_all(ds.driver, schema.as_deref(), &t.name),
                            params: Vec::new(),
                        },
                        context: None,
                        path: path.join(format!("{}.{extension}", file_stem(&t.name))),
                        table: TableName { schema: schema.clone(), table: t.name },
                    })
                    .collect();
                let title = format!(
                    "Export {}",
                    schema.clone().unwrap_or_else(|| ds.display_name())
                );
                let task = self.begin_transfer(TransferKind::Export, title, Some(path), None);
                self.backend.export_data(task, ExportJob { items, options });
            }
        }
    }
}

fn settings(ui: &mut Ui, dialog: &mut ExportDialog) {
    theme::section_header(ui, "Source");
    ui.label(RichText::new(dialog.source_text()).color(theme::TEXT));
    if let ExportSource::Schema { .. } = dialog.source {
        ui.add_space(4.0);
        table_list(ui, dialog);
    }
    theme::section_header(ui, "Extractor");
    theme::form_row(ui, "Format:", LABEL_WIDTH, |ui| {
        let mut format = dialog.options.format;
        egui::ComboBox::from_id_salt("export_format")
            .selected_text(format.label())
            .width(170.0)
            .show_ui(ui, |ui| {
                for option in ExportFormat::ALL {
                    ui.selectable_value(&mut format, option, option.label());
                }
            });
        if format != dialog.options.format {
            dialog.set_format(format);
        }
    });
    let options = &mut dialog.options;
    if options.format == ExportFormat::Csv {
        theme::form_row(ui, "Separator:", LABEL_WIDTH, |ui| {
            let named = [(",", "Comma"), (";", "Semicolon"), ("|", "Pipe")];
            let selected = named
                .iter()
                .find(|(s, _)| *s == options.separator)
                .map_or("Custom", |(_, name)| name);
            egui::ComboBox::from_id_salt("export_separator")
                .selected_text(selected)
                .width(110.0)
                .show_ui(ui, |ui| {
                    for (separator, name) in named {
                        if ui.selectable_label(options.separator == separator, name).clicked() {
                            options.separator = separator.into();
                        }
                    }
                });
            ui.add(egui::TextEdit::singleline(&mut options.separator).desired_width(40.0));
        });
    }
    if options.format.delimited() {
        theme::form_row(ui, "Quote values:", LABEL_WIDTH, |ui| {
            egui::ComboBox::from_id_salt("export_quote")
                .selected_text(options.quote.label())
                .width(170.0)
                .show_ui(ui, |ui| {
                    for quote in Quote::ALL {
                        ui.selectable_value(&mut options.quote, quote, quote.label());
                    }
                });
        });
        theme::form_row(ui, "NULL value text:", LABEL_WIDTH, |ui| {
            ui.add(egui::TextEdit::singleline(&mut options.null_text).desired_width(170.0).hint_text("empty"));
        });
        theme::form_row(ui, "Line separator:", LABEL_WIDTH, |ui| {
            egui::ComboBox::from_id_salt("export_line_ending")
                .selected_text(match options.line_ending {
                    LineEnding::Lf => "LF (\\n)",
                    LineEnding::CrLf => "CRLF (\\r\\n)",
                })
                .width(170.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut options.line_ending, LineEnding::Lf, "LF (\\n)");
                    ui.selectable_value(&mut options.line_ending, LineEnding::CrLf, "CRLF (\\r\\n)");
                });
        });
    }
    if options.format.has_header_option() {
        theme::form_row(ui, "", LABEL_WIDTH, |ui| {
            ui.checkbox(&mut options.header, "Add column header");
        });
    }
    if options.format == ExportFormat::SqlInsert {
        theme::form_row(ui, "Rows per statement:", LABEL_WIDTH, |ui| {
            ui.add(egui::DragValue::new(&mut options.rows_per_insert).range(1..=1000).speed(1.0));
            theme::help_icon(ui, "1 writes one INSERT per row; more rows per statement load faster.");
        });
        theme::form_row(ui, "", LABEL_WIDTH, |ui| {
            ui.checkbox(&mut options.qualify_table, "Qualify table name with schema");
        });
    }
    theme::section_header(ui, "Output");
    let folder = matches!(dialog.source, ExportSource::Schema { .. });
    theme::form_row(ui, if folder { "Folder:" } else { "File:" }, LABEL_WIDTH, |ui| {
        ui.add(egui::TextEdit::singleline(&mut dialog.path).desired_width((ui.available_width() - 96.0).max(120.0)));
        if theme::dialog_button(ui, "Browse…").clicked() {
            let current = PathBuf::from(dialog.path.trim());
            let picked = if folder {
                rfd::FileDialog::new().set_directory(&current).pick_folder()
            } else {
                let mut picker = rfd::FileDialog::new()
                    .add_filter(dialog.options.format.label(), &[dialog.options.format.extension()]);
                if let Some(parent) = current.parent() {
                    picker = picker.set_directory(parent);
                }
                if let Some(name) = current.file_name() {
                    picker = picker.set_file_name(name.to_string_lossy());
                }
                picker.save_file()
            };
            if let Some(path) = picked {
                dialog.path = path.to_string_lossy().into_owned();
            }
        }
    });
    if folder {
        ui.label(
            RichText::new(format!("One file per table: name.{}", dialog.options.format.extension()))
                .small()
                .color(theme::TEXT_MUTED),
        );
    }
}

fn table_list(ui: &mut Ui, dialog: &mut ExportDialog) {
    match &mut dialog.tables {
        Tables::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Reading tables…").color(theme::TEXT_MUTED));
            });
        }
        Tables::Failed(error) => {
            ui.label(RichText::new(error.as_str()).color(theme::RED));
        }
        Tables::Loaded(tables) => {
            ui.horizontal(|ui| {
                theme::flat_widgets(ui);
                if ui.small_button("Select All").clicked() {
                    tables.iter_mut().for_each(|t| t.chosen = true);
                }
                if ui.small_button("Select None").clicked() {
                    tables.iter_mut().for_each(|t| t.chosen = false);
                }
                let chosen = tables.iter().filter(|t| t.chosen).count();
                ui.label(RichText::new(format!("{chosen} of {}", tables.len())).small().color(theme::TEXT_MUTED));
            });
            egui::Frame::new()
                .fill(theme::BG_EDITOR)
                .corner_radius(4)
                .inner_margin(6)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("export_tables")
                        .max_height(180.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for table in tables.iter_mut() {
                                let label = if table.view { format!("{} (view)", table.name) } else { table.name.clone() };
                                ui.checkbox(&mut table.chosen, label);
                            }
                        });
                });
        }
    }
}

fn preview(ui: &mut Ui, dialog: &mut ExportDialog) {
    ui.label(RichText::new("Preview").font(theme::semibold(13.0)));
    ui.add_space(4.0);
    let error = dialog.preview_error.clone();
    let text = dialog.preview_text().map(str::to_string);
    egui::Frame::new()
        .fill(theme::BG_EDITOR)
        .corner_radius(4)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match (text, error) {
                (Some(text), _) => {
                    egui::ScrollArea::both()
                        .id_salt("export_preview")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(text).monospace()).extend());
                        });
                }
                (None, Some(error)) => {
                    ui.label(RichText::new(error).color(theme::RED));
                }
                (None, None) if matches!(dialog.source, ExportSource::Schema { .. }) => {
                    theme::empty_text(ui, "Each chosen table is written to its own file");
                }
                (None, None) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Reading the first rows…").color(theme::TEXT_MUTED));
                    });
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_avoid_reserved_characters() {
        assert_eq!(file_stem("orders"), "orders");
        assert_eq!(file_stem("a/b:c*?"), "a_b_c__");
        assert_eq!(file_stem("   "), "export");
    }
}
