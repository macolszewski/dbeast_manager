//! Import Data from File dialog: CSV format
//! settings and the target on the left; column mapping, a preview of the
//! records and the DDL of a new table on the right.

use super::export_dialog::{NodeScope, transfer_dir};
use super::{App, theme, transfers::TransferKind};
use crate::backend::{ImportJob, OnError};
use crate::import::{CsvOptions, CsvPreview, InferredType, create_table_sql, table_name_for};
use crate::model::{DataSource, DriverKind};
use egui::{Context, Id, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use std::path::{Path, PathBuf};

const LABEL_WIDTH: f32 = 112.0;
const SKIP: &str = "<skip>";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    New,
    Existing,
}

/// A column of a new table: whether to import it, its name and SQL type.
struct NewColumn {
    include: bool,
    name: String,
    ty: String,
}

pub(super) struct ImportDialog {
    ds: DataSource,
    schema: Option<String>,
    path: String,
    csv: CsvOptions,
    /// Options and file of the preview shown or requested last.
    previewed: Option<(String, CsvOptions)>,
    detect: bool,
    preview: Option<CsvPreview>,
    preview_error: Option<String>,
    preview_request: u64,
    target: Target,
    new_table: String,
    new_columns: Vec<NewColumn>,
    tables: Option<Result<Vec<(String, bool)>, String>>,
    tables_request: u64,
    existing: Option<String>,
    target_columns: Option<Result<Vec<(String, String)>, String>>,
    columns_request: u64,
    /// Source field of each target column of an existing table.
    mapping: Vec<Option<usize>>,
    clear: bool,
    skip_errors: bool,
    max_errors: u64,
    write_errors: bool,
}

impl ImportDialog {
    fn driver(&self) -> DriverKind {
        self.ds.driver
    }

    fn file_name(&self) -> String {
        Path::new(self.path.trim())
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Rebuilds new-table columns when the file's columns change, keeping edits.
    fn sync_new_columns(&mut self) {
        let Some(preview) = &self.preview else {
            return;
        };
        if self.new_columns.len() == preview.columns.len() {
            return;
        }
        let driver = self.driver();
        self.new_columns = preview
            .columns
            .iter()
            .zip(&preview.types)
            .map(|(name, ty)| NewColumn {
                include: true,
                name: name.clone(),
                ty: ty.sql(driver).to_string(),
            })
            .collect();
    }

    /// Maps target columns to source fields by name, then by position.
    fn auto_map(&mut self) {
        let (Some(preview), Some(Ok(columns))) = (&self.preview, &self.target_columns) else {
            return;
        };
        let by_name = |name: &str| preview.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
        let any_name = columns.iter().any(|(name, _)| by_name(name).is_some());
        self.mapping = columns
            .iter()
            .enumerate()
            .map(|(i, (name, _))| {
                if any_name {
                    by_name(name)
                } else {
                    (i < preview.columns.len()).then_some(i)
                }
            })
            .collect();
    }

    fn ddl(&self) -> Option<String> {
        let columns: Vec<(String, String)> = self
            .new_columns
            .iter()
            .filter(|c| c.include)
            .map(|c| (c.name.trim().to_string(), c.ty.trim().to_string()))
            .collect();
        (!columns.is_empty()).then(|| create_table_sql(self.driver(), self.schema.as_deref(), self.new_table.trim(), &columns))
    }

    fn blocked(&self) -> Option<String> {
        if self.preview.is_none() {
            return Some(self.preview_error.clone().unwrap_or_else(|| "Reading the file…".into()));
        }
        match self.target {
            Target::New => {
                if self.new_table.trim().is_empty() {
                    return Some("Enter the table name".into());
                }
                let included: Vec<&NewColumn> = self.new_columns.iter().filter(|c| c.include).collect();
                if included.is_empty() {
                    return Some("Choose at least one column".into());
                }
                if included.iter().any(|c| c.name.trim().is_empty() || c.ty.trim().is_empty()) {
                    return Some("Every column needs a name and a type".into());
                }
                let mut names: Vec<String> = included.iter().map(|c| c.name.trim().to_lowercase()).collect();
                names.sort();
                if names.windows(2).any(|w| w[0] == w[1]) {
                    return Some("Column names must be unique".into());
                }
            }
            Target::Existing => {
                if self.existing.is_none() {
                    return Some("Choose the target table".into());
                }
                match &self.target_columns {
                    Some(Ok(_)) if self.mapping.iter().any(Option::is_some) => {}
                    Some(Ok(_)) => return Some("Map at least one column".into()),
                    Some(Err(error)) => return Some(error.clone()),
                    None => return Some("Reading the table columns…".into()),
                }
            }
        }
        None
    }

    fn job(&self) -> Option<ImportJob> {
        let path = PathBuf::from(self.path.trim());
        let (table, create, columns, sources) = match self.target {
            Target::New => {
                let included: Vec<(usize, &NewColumn)> = self.new_columns.iter().enumerate().filter(|(_, c)| c.include).collect();
                (
                    self.new_table.trim().to_string(),
                    self.ddl(),
                    included.iter().map(|(_, c)| c.name.trim().to_string()).collect(),
                    included.iter().map(|(i, _)| *i).collect(),
                )
            }
            Target::Existing => {
                let Some(Ok(target)) = &self.target_columns else {
                    return None;
                };
                let pairs: Vec<(String, usize)> = target
                    .iter()
                    .zip(&self.mapping)
                    .filter_map(|((name, _), source)| source.map(|s| (name.clone(), s)))
                    .collect();
                (
                    self.existing.clone()?,
                    None,
                    pairs.iter().map(|(n, _)| n.clone()).collect(),
                    pairs.iter().map(|(_, s)| *s).collect(),
                )
            }
        };
        let errors_file = (self.skip_errors && self.write_errors).then(|| {
            let stem = path.file_stem().map_or_else(|| "import".into(), |s| s.to_string_lossy().into_owned());
            path.with_file_name(format!("{stem}.errors.csv"))
        });
        Some(ImportJob {
            ds: self.ds.clone(),
            path,
            csv: self.csv.clone(),
            schema: self.schema.clone(),
            table,
            create,
            columns,
            sources,
            clear: self.target == Target::Existing && self.clear,
            on_error: if self.skip_errors { OnError::Skip { max: self.max_errors } } else { OnError::Abort },
            errors_file,
        })
    }
}

impl App {
    /// Asks for the file first, then opens the dialog.
    pub(super) fn open_import_dialog(&mut self, scope: NodeScope) {
        let picked = rfd::FileDialog::new()
            .add_filter("CSV/TSV", &["csv", "tsv", "txt"])
            .set_directory(transfer_dir(&self.config.transfer_dir))
            .pick_file();
        if let Some(path) = picked {
            self.open_import_dialog_for(scope, path);
        }
    }

    /// Opens the Import dialog for a file without the system file picker,
    /// which UI tests cannot drive.
    #[doc(hidden)]
    pub fn import_file_for_test(&mut self, ds_id: &str, table: Option<&str>, path: PathBuf) {
        let Some(ds) = self.source(ds_id).cloned() else {
            return;
        };
        let scope = NodeScope { ds, schema: None, table: table.map(str::to_string) };
        self.open_import_dialog_for(scope, path);
    }

    pub(super) fn open_import_dialog_for(&mut self, scope: NodeScope, path: PathBuf) {
        let csv = CsvOptions {
            delimiter: b',',
            ..CsvOptions::default()
        };
        let mut dialog = ImportDialog {
            ds: scope.ds,
            schema: scope.schema,
            path: path.to_string_lossy().into_owned(),
            csv,
            previewed: None,
            detect: true,
            preview: None,
            preview_error: None,
            preview_request: 0,
            target: if scope.table.is_some() { Target::Existing } else { Target::New },
            new_table: table_name_for(&path),
            new_columns: Vec::new(),
            tables: None,
            tables_request: 0,
            existing: scope.table.clone(),
            target_columns: None,
            columns_request: 0,
            mapping: Vec::new(),
            clear: false,
            skip_errors: false,
            max_errors: 100,
            write_errors: true,
        };
        self.transfer_request += 1;
        dialog.tables_request = self.transfer_request;
        self.backend.transfer_tables(dialog.tables_request, dialog.ds.clone(), dialog.schema.clone());
        if let Some(table) = scope.table {
            self.transfer_request += 1;
            dialog.columns_request = self.transfer_request;
            self.backend.import_columns(dialog.columns_request, dialog.ds.clone(), dialog.schema.clone(), table);
        }
        self.import_dialog = Some(dialog);
        self.request_import_preview();
    }

    /// Parses the file again when it or the format settings changed.
    fn request_import_preview(&mut self) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        if let Some((path, csv)) = &dialog.previewed
            && path == dialog.path.trim()
            && *csv == dialog.csv
        {
            return;
        }
        dialog.previewed = Some((dialog.path.trim().to_string(), dialog.csv.clone()));
        self.transfer_request += 1;
        dialog.preview_request = self.transfer_request;
        let detect = std::mem::take(&mut dialog.detect);
        self.backend.import_preview(dialog.preview_request, PathBuf::from(dialog.path.trim()), dialog.csv.clone(), detect);
    }

    pub(super) fn import_preview_loaded(&mut self, request: u64, result: Result<CsvPreview, String>) {
        let Some(dialog) = self.import_dialog.as_mut().filter(|d| d.preview_request == request) else {
            return;
        };
        match result {
            Ok(preview) => {
                if let Some(delimiter) = preview.detected_delimiter {
                    dialog.csv.delimiter = delimiter;
                    if let Some((_, csv)) = dialog.previewed.as_mut() {
                        csv.delimiter = delimiter;
                    }
                }
                let columns_changed = dialog.preview.as_ref().map(|p| &p.columns) != Some(&preview.columns);
                dialog.preview = Some(preview);
                dialog.preview_error = None;
                if columns_changed {
                    dialog.new_columns.clear();
                    dialog.sync_new_columns();
                    dialog.auto_map();
                }
            }
            Err(error) => {
                dialog.preview = None;
                dialog.preview_error = Some(error);
            }
        }
    }

    pub(super) fn import_tables_loaded(&mut self, request: u64, result: &Result<Vec<(String, bool)>, String>) -> bool {
        let Some(dialog) = self.import_dialog.as_mut().filter(|d| d.tables_request == request) else {
            return false;
        };
        dialog.tables = Some(result.as_ref().map(|t| t.iter().filter(|(_, view)| !view).cloned().collect()).map_err(Clone::clone));
        true
    }

    pub(super) fn import_columns_loaded(&mut self, request: u64, result: Result<Vec<(String, String)>, String>) {
        let Some(dialog) = self.import_dialog.as_mut().filter(|d| d.columns_request == request) else {
            return;
        };
        dialog.target_columns = Some(result);
        dialog.auto_map();
    }

    pub(super) fn import_dialog_window(&mut self, ctx: &Context) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        let (mut import, mut cancel) = (false, false);
        let mut load_columns = None;
        let title = format!("Import \"{}\"", dialog.file_name());
        let response = theme::Dialog::new(Id::new("import_data_dialog"), title)
            .default_size([980.0, 600.0])
            .min_size([640.0, 420.0])
            .show(ctx, |ui| {
                egui::Panel::bottom("import_data_footer")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(egui::Frame::new())
                    .show(ui, |ui| {
                        let blocked = dialog.blocked();
                        theme::dialog_footer(
                            ui,
                            |ui| {
                                if let Some(reason) = &blocked {
                                    ui.label(RichText::new(reason).color(theme::TEXT_MUTED));
                                }
                            },
                            |ui| {
                                cancel = theme::dialog_button(ui, "Cancel").clicked();
                                ui.add_enabled_ui(blocked.is_none(), |ui| {
                                    import = theme::primary_button(ui, "Import").clicked();
                                });
                            },
                        );
                    });
                egui::Panel::left("import_data_settings")
                    .resizable(true)
                    .default_size(370.0)
                    .size_range(320.0..=520.0)
                    .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 12, top: 0, bottom: 0 }))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("import_settings")
                            .show(ui, |ui| load_columns = settings(ui, dialog));
                    });
                columns_and_preview(ui, dialog);
            });
        if response.should_close() {
            cancel = true;
        }
        if let Some(table) = load_columns {
            self.transfer_request += 1;
            let request = self.transfer_request;
            if let Some(dialog) = self.import_dialog.as_mut() {
                dialog.columns_request = request;
                dialog.target_columns = None;
                self.backend.import_columns(request, dialog.ds.clone(), dialog.schema.clone(), table);
            }
        }
        if cancel {
            self.import_dialog = None;
            return;
        }
        if import {
            let dialog = self.import_dialog.take().expect("open dialog");
            if let Some(job) = dialog.job() {
                self.config.transfer_dir = job.path.parent().map(Path::to_path_buf);
                self.save_config();
                let task = self.begin_transfer(
                    TransferKind::Import,
                    format!("Import {}", dialog.file_name()),
                    job.errors_file.clone(),
                    None,
                );
                self.backend.import_data(task, job);
            }
            return;
        }
        self.request_import_preview();
    }
}

/// Settings column; returns a table whose columns must be read.
fn settings(ui: &mut Ui, dialog: &mut ImportDialog) -> Option<String> {
    let mut load_columns = None;
    theme::section_header(ui, "File");
    theme::form_row(ui, "File:", LABEL_WIDTH, |ui| {
        ui.add(egui::TextEdit::singleline(&mut dialog.path).desired_width((ui.available_width() - 96.0).max(120.0)));
        if theme::dialog_button(ui, "Browse…").clicked()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("CSV/TSV", &["csv", "tsv", "txt"])
                .set_directory(Path::new(dialog.path.trim()).parent().unwrap_or(Path::new(".")))
                .pick_file()
        {
            dialog.path = path.to_string_lossy().into_owned();
            dialog.detect = true;
            if dialog.target == Target::New {
                dialog.new_table = table_name_for(&path);
            }
        }
    });
    if let Some(preview) = &dialog.preview {
        theme::form_row(ui, "", LABEL_WIDTH, |ui| {
            ui.label(
                RichText::new(format!("{} · UTF-8", crate::backend::bytes_text(preview.file_size)))
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        });
    }
    theme::section_header(ui, "Format");
    let csv = &mut dialog.csv;
    theme::form_row(ui, "Separator:", LABEL_WIDTH, |ui| {
        let named = [(b',', "Comma"), (b';', "Semicolon"), (b'\t', "Tab"), (b'|', "Pipe")];
        let selected = named.iter().find(|(d, _)| *d == csv.delimiter).map_or("Custom", |(_, n)| n);
        egui::ComboBox::from_id_salt("import_separator")
            .selected_text(selected)
            .width(110.0)
            .show_ui(ui, |ui| {
                for (delimiter, name) in named {
                    ui.selectable_value(&mut csv.delimiter, delimiter, name);
                }
            });
        if !named.iter().any(|(d, _)| *d == csv.delimiter) || selected == "Custom" {
            let mut text = (csv.delimiter as char).to_string();
            if ui.add(egui::TextEdit::singleline(&mut text).desired_width(30.0).char_limit(1)).changed()
                && let Some(b) = text.bytes().next()
            {
                csv.delimiter = b;
            }
        }
    });
    theme::form_row(ui, "Quote:", LABEL_WIDTH, |ui| {
        let mut text = (csv.quote as char).to_string();
        if ui.add(egui::TextEdit::singleline(&mut text).desired_width(30.0).char_limit(1)).changed()
            && let Some(b) = text.bytes().next()
        {
            csv.quote = b;
        }
    });
    theme::form_row(ui, "NULL value text:", LABEL_WIDTH, |ui| {
        ui.add(egui::TextEdit::singleline(&mut csv.null_text).desired_width(140.0).hint_text("empty"));
        theme::help_icon(ui, "Fields equal to this text become NULL. Empty means empty fields are NULL.");
    });
    theme::form_row(ui, "Skip lines:", LABEL_WIDTH, |ui| {
        ui.add(egui::DragValue::new(&mut csv.skip_lines).range(0..=1000));
    });
    theme::form_row(ui, "", LABEL_WIDTH, |ui| {
        ui.checkbox(&mut csv.header, "First row is header");
    });
    theme::form_row(ui, "", LABEL_WIDTH, |ui| {
        ui.checkbox(&mut csv.trim, "Trim whitespace");
    });
    theme::section_header(ui, "Target");
    if let Some(schema) = &dialog.schema {
        theme::form_row(ui, "Schema:", LABEL_WIDTH, |ui| {
            ui.label(schema.as_str());
        });
    }
    theme::form_row(ui, "", LABEL_WIDTH, |ui| {
        ui.radio_value(&mut dialog.target, Target::New, "New table");
        ui.radio_value(&mut dialog.target, Target::Existing, "Existing table");
    });
    match dialog.target {
        Target::New => {
            theme::form_row(ui, "Table:", LABEL_WIDTH, |ui| {
                ui.add(egui::TextEdit::singleline(&mut dialog.new_table).desired_width(200.0));
            });
            if let Some(Ok(tables)) = &dialog.tables
                && tables.iter().any(|(name, _)| name.eq_ignore_ascii_case(dialog.new_table.trim()))
            {
                theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                    ui.label(RichText::new("A table with this name exists").small().color(theme::SEARCH_MARKER));
                });
            }
        }
        Target::Existing => {
            theme::form_row(ui, "Table:", LABEL_WIDTH, |ui| match &dialog.tables {
                None => {
                    ui.spinner();
                }
                Some(Err(error)) => {
                    ui.label(RichText::new(error.as_str()).color(theme::RED));
                }
                Some(Ok(tables)) => {
                    let before = dialog.existing.clone();
                    egui::ComboBox::from_id_salt("import_existing")
                        .selected_text(dialog.existing.as_deref().unwrap_or("Choose a table"))
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for (name, _) in tables {
                                ui.selectable_value(&mut dialog.existing, Some(name.clone()), name);
                            }
                        });
                    if dialog.existing != before {
                        load_columns = dialog.existing.clone();
                    }
                }
            });
            theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                ui.checkbox(&mut dialog.clear, "Delete existing rows before import");
            });
        }
    }
    theme::section_header(ui, "Errors");
    theme::form_row(ui, "On error:", LABEL_WIDTH, |ui| {
        egui::ComboBox::from_id_salt("import_on_error")
            .selected_text(if dialog.skip_errors { "Skip row" } else { "Stop and roll back" })
            .width(160.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut dialog.skip_errors, false, "Stop and roll back");
                ui.selectable_value(&mut dialog.skip_errors, true, "Skip row");
            });
    });
    if dialog.skip_errors {
        theme::form_row(ui, "Max errors:", LABEL_WIDTH, |ui| {
            ui.add(egui::DragValue::new(&mut dialog.max_errors).range(1..=1_000_000));
        });
        theme::form_row(ui, "", LABEL_WIDTH, |ui| {
            ui.checkbox(&mut dialog.write_errors, "Write errors to file");
            theme::help_icon(ui, "Skipped records and their errors go to name.errors.csv next to the file.");
        });
    }
    load_columns
}

fn columns_and_preview(ui: &mut Ui, dialog: &mut ImportDialog) {
    // Taken out while the column editors borrow the dialog mutably.
    let Some(preview) = dialog.preview.take() else {
        match &dialog.preview_error {
            Some(error) => {
                ui.label(RichText::new(error.as_str()).color(theme::RED));
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Reading the file…").color(theme::TEXT_MUTED));
                });
            }
        }
        return;
    };
    let height = ui.available_height();
    ui.label(RichText::new("Columns").font(theme::semibold(13.0)));
    ui.add_space(4.0);
    egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(4).inner_margin(6).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical()
            .id_salt("import_columns")
            .max_height((height * 0.42).max(120.0))
            .auto_shrink([false, true])
            .show(ui, |ui| match dialog.target {
                Target::New => new_columns(ui, dialog, &preview),
                Target::Existing => mapping(ui, dialog, &preview),
            });
    });
    if dialog.target == Target::New
        && let Some(ddl) = dialog.ddl()
    {
        egui::CollapsingHeader::new("DDL").id_salt("import_ddl").show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(ddl).monospace().color(theme::TEXT_MUTED)).selectable(true));
        });
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Preview").font(theme::semibold(13.0)));
        ui.label(
            RichText::new(format!("first {} records", preview.rows.len()))
                .small()
                .color(theme::TEXT_MUTED),
        );
    });
    ui.add_space(4.0);
    preview_grid(ui, &preview);
    dialog.preview = Some(preview);
}

fn new_columns(ui: &mut Ui, dialog: &mut ImportDialog, preview: &CsvPreview) {
    let driver = dialog.driver();
    let types = [
        InferredType::Integer,
        InferredType::BigInt,
        InferredType::Decimal,
        InferredType::Double,
        InferredType::Boolean,
        InferredType::Date,
        InferredType::Timestamp,
        InferredType::Text,
    ];
    let names = &preview.columns;
    egui::Grid::new("import_new_columns")
        .num_columns(4)
        .spacing([10.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for header in ["", "File column", "Name", "Type"] {
                ui.label(RichText::new(header).small().color(theme::TEXT_MUTED));
            }
            ui.end_row();
            for (i, column) in dialog.new_columns.iter_mut().enumerate() {
                ui.checkbox(&mut column.include, "");
                ui.label(names.get(i).map_or("", String::as_str));
                ui.add_enabled_ui(column.include, |ui| {
                    ui.add_sized([170.0, 22.0], egui::TextEdit::singleline(&mut column.name));
                });
                ui.add_enabled_ui(column.include, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.add_sized([140.0, 22.0], egui::TextEdit::singleline(&mut column.ty));
                        theme::flat_widgets(ui);
                        ui.menu_button("⏷", |ui| {
                            for ty in types {
                                if ui.selectable_label(column.ty == ty.sql(driver), ty.sql(driver)).clicked() {
                                    column.ty = ty.sql(driver).to_string();
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text("Common types");
                    });
                });
                ui.end_row();
            }
        });
}

fn mapping(ui: &mut Ui, dialog: &mut ImportDialog, preview: &CsvPreview) {
    match &dialog.target_columns {
        None => {
            if dialog.existing.is_some() {
                ui.spinner();
            } else {
                ui.label(RichText::new("Choose the target table").color(theme::TEXT_MUTED));
            }
        }
        Some(Err(error)) => {
            ui.label(RichText::new(error.as_str()).color(theme::RED));
        }
        Some(Ok(columns)) => {
            if dialog.mapping.len() != columns.len() {
                dialog.mapping.resize(columns.len(), None);
            }
            egui::Grid::new("import_mapping")
                .num_columns(3)
                .spacing([10.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    for header in ["Table column", "Type", "From file column"] {
                        ui.label(RichText::new(header).small().color(theme::TEXT_MUTED));
                    }
                    ui.end_row();
                    for (i, (name, ty)) in columns.iter().enumerate() {
                        ui.label(name.as_str());
                        ui.label(RichText::new(ty.as_str()).color(theme::TEXT_MUTED));
                        let source = &mut dialog.mapping[i];
                        let selected = source.and_then(|s| preview.columns.get(s)).map_or(SKIP, String::as_str);
                        egui::ComboBox::from_id_salt(("import_map", i))
                            .selected_text(selected)
                            .width(170.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(source, None, SKIP);
                                for (s, source_name) in preview.columns.iter().enumerate() {
                                    ui.selectable_value(source, Some(s), source_name);
                                }
                            });
                        ui.end_row();
                    }
                });
        }
    }
}

fn preview_grid(ui: &mut Ui, preview: &CsvPreview) {
    egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(4).inner_margin(4).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        let font = theme::grid_font(ui);
        egui::ScrollArea::horizontal().id_salt("import_preview").show(ui, |ui| {
            let mut table = TableBuilder::new(ui)
                .id_salt("import_preview_table")
                .striped(true)
                .resizable(true)
                .auto_shrink([false, false])
                .min_scrolled_height(0.0);
            for _ in &preview.columns {
                table = table.column(Column::initial(120.0).at_least(40.0).clip(true));
            }
            table
                .header(24.0, |mut header| {
                    for (name, ty) in preview.columns.iter().zip(&preview.types) {
                        header.col(|ui| {
                            ui.label(RichText::new(name).strong())
                                .on_hover_text(format!("Detected: {ty:?}"));
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, preview.rows.len(), |mut row| {
                        let record = &preview.rows[row.index()];
                        for c in 0..preview.columns.len() {
                            row.col(|ui| {
                                let text = record.get(c).map_or("", String::as_str);
                                ui.add(egui::Label::new(RichText::new(text.replace('\n', "↵")).font(font.clone())).truncate());
                            });
                        }
                    });
                });
        });
    });
}
