//! "Export with 'pg_dump'", "Restore with 'pg_restore'" and "Restore with
//! 'psql'" dialogs: the tool, the objects or file, the options and the exact
//! command line, which runs as a transfer in the Run tool window.

use super::export_dialog::{NodeScope, file_stem, transfer_dir};
use super::{App, theme, transfers::TransferKind};
use crate::backend::{ToolJob, ToolTask};
use crate::dump::{self, Content, DumpFormat, DumpOptions, DumpTarget, Endpoint, RestoreOptions, Statements, Tool};
use crate::model::{DataSource, ObjectKind};
use egui::{Context, Id, RichText, Ui};
use std::path::{Path, PathBuf};

const LABEL_WIDTH: f32 = 130.0;

#[derive(Clone, PartialEq)]
enum Mode {
    Dump { options: DumpOptions, schemas: String, tables: String },
    Restore { options: RestoreOptions, psql: bool },
}

pub(super) struct DumpDialog {
    ds: DataSource,
    mode: Mode,
    tool_path: String,
    /// Tool found for the current path and its `--version`.
    resolved: Option<PathBuf>,
    /// `tool_path` the tool was last looked up for; the lookup scans PATH.
    resolved_for: Option<String>,
    version: Option<Result<String, String>>,
    version_request: u64,
    /// Output of a dump, input of a restore.
    path: String,
    /// Job and command line for the inputs they were built from, rebuilt
    /// only when an input changes rather than on every frame.
    prepared: Option<(JobInputs, Result<ToolJob, String>, Option<String>)>,
}

#[derive(PartialEq)]
struct JobInputs {
    program: Option<PathBuf>,
    path: String,
    mode: Mode,
}

impl DumpDialog {
    fn tool(&self) -> Tool {
        match self.mode {
            Mode::Dump { .. } => Tool::PgDump,
            Mode::Restore { psql: false, .. } => Tool::PgRestore,
            Mode::Restore { psql: true, .. } => Tool::Psql,
        }
    }

    fn title(&self) -> String {
        match self.mode {
            Mode::Dump { .. } => "Export with 'pg_dump'".into(),
            Mode::Restore { psql: false, .. } => "Restore with 'pg_restore'".into(),
            Mode::Restore { psql: true, .. } => "Restore with 'psql'".into(),
        }
    }

    fn database(&self) -> String {
        Endpoint::of(&self.ds).database
    }

    fn job(&self) -> Result<ToolJob, String> {
        let program = self.resolved.clone().ok_or_else(|| format!("{} was not found; choose its path", self.tool().binary()))?;
        let path = PathBuf::from(self.path.trim());
        if self.path.trim().is_empty() {
            return Err(match self.mode {
                Mode::Dump { .. } => "Choose where to write the dump".into(),
                Mode::Restore { .. } => "Choose the file to restore".into(),
            });
        }
        let task = match &self.mode {
            Mode::Dump { options, schemas, tables } => ToolTask::Dump {
                options: options.clone(),
                target: DumpTarget { output: path, schemas: list(schemas), tables: list(tables) },
            },
            Mode::Restore { options, psql: false } => ToolTask::Restore { options: options.clone(), input: path },
            Mode::Restore { options, psql: true } => ToolTask::Psql { options: options.clone(), input: path },
        };
        if let ToolTask::Restore { input, .. } | ToolTask::Psql { input, .. } = &task
            && !input.exists()
        {
            return Err(format!("{} does not exist", input.display()));
        }
        dump::environment(&self.ds)?;
        let job = ToolJob { program, ds: self.ds.clone(), task };
        job.args(&Endpoint::of(&self.ds), None)?;
        Ok(job)
    }

    /// Rebuilds the job and its command line when the inputs changed.
    fn prepare(&mut self) {
        let inputs = JobInputs { program: self.resolved.clone(), path: self.path.clone(), mode: self.mode.clone() };
        if self.prepared.as_ref().is_some_and(|(prepared, ..)| *prepared == inputs) {
            return;
        }
        let job = self.job();
        let line = job
            .as_ref()
            .ok()
            .and_then(|job| Some(dump::command_line(&job.program, &job.args(&Endpoint::of(&self.ds), None).ok()?)));
        self.prepared = Some((inputs, job, line));
    }
}

/// Comma-separated names from a field.
fn list(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

impl App {
    pub(super) fn open_dump_dialog(&mut self, scope: NodeScope, kind: Option<ObjectKind>) {
        let options = self.config.dump_options.clone();
        let (schemas, tables) = match (kind, &scope.schema, &scope.table) {
            (Some(ObjectKind::Table | ObjectKind::View), Some(schema), Some(table)) => (String::new(), format!("{schema}.{table}")),
            (Some(ObjectKind::Schema), Some(schema), _) => (schema.clone(), String::new()),
            _ => (String::new(), String::new()),
        };
        let name = scope
            .table
            .clone()
            .or_else(|| scope.schema.clone())
            .unwrap_or_else(|| Endpoint::of(&scope.ds).database);
        let path = dump_path(&transfer_dir(&self.config.transfer_dir), &name, options.format);
        let dialog = DumpDialog {
            tool_path: self.config.pg_tools.pg_dump.clone(),
            ds: scope.ds,
            mode: Mode::Dump { options, schemas, tables },
            resolved: None,
            resolved_for: None,
            version: None,
            version_request: 0,
            path: path.to_string_lossy().into_owned(),
            prepared: None,
        };
        self.dump_dialog = Some(dialog);
        self.resolve_tool();
    }

    pub(super) fn open_restore_dialog(&mut self, scope: NodeScope, psql: bool) {
        let tool = if psql { Tool::Psql } else { Tool::PgRestore };
        let dialog = DumpDialog {
            tool_path: self.config.pg_tools.get(tool).to_string(),
            ds: scope.ds,
            mode: Mode::Restore { options: self.config.restore_options.clone(), psql },
            resolved: None,
            resolved_for: None,
            version: None,
            version_request: 0,
            path: String::new(),
            prepared: None,
        };
        self.dump_dialog = Some(dialog);
        self.resolve_tool();
    }

    /// Finds the tool for the current path and asks for its version.
    fn resolve_tool(&mut self) {
        let Some(dialog) = self.dump_dialog.as_mut() else {
            return;
        };
        if dialog.resolved_for.as_ref() == Some(&dialog.tool_path) {
            return;
        }
        dialog.resolved_for = Some(dialog.tool_path.clone());
        let resolved = dump::find_tool(dialog.tool(), &dialog.tool_path);
        if resolved == dialog.resolved && dialog.version.is_some() {
            return;
        }
        dialog.resolved = resolved.clone();
        match resolved {
            Some(program) => {
                self.transfer_request += 1;
                dialog.version_request = self.transfer_request;
                dialog.version = Some(Ok(String::new()));
                self.backend.tool_version(dialog.version_request, program);
            }
            None => dialog.version = Some(Err(format!("{} not found", dialog.tool().binary()))),
        }
    }

    pub(super) fn tool_version_loaded(&mut self, request: u64, result: Result<String, String>) {
        if let Some(dialog) = self.dump_dialog.as_mut().filter(|d| d.version_request == request) {
            dialog.version = Some(result);
        }
    }

    pub(super) fn dump_dialog_window(&mut self, ctx: &Context) {
        let Some(dialog) = self.dump_dialog.as_mut() else {
            return;
        };
        let (mut run, mut cancel) = (false, false);
        dialog.prepare();
        // Taken out while the settings below borrow the dialog mutably.
        let prepared = dialog.prepared.take().expect("prepared above");
        let (_, job, line) = &prepared;
        let response = theme::Dialog::new(Id::new("pg_tool_dialog"), dialog.title())
            .default_size([640.0, 640.0])
            .min_size([480.0, 420.0])
            .show(ctx, |ui| {
                egui::Panel::bottom("pg_tool_footer")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(egui::Frame::new())
                    .show(ui, |ui| {
                        command_preview(ui, line.as_deref());
                        theme::dialog_footer(
                            ui,
                            |ui| {
                                if let Err(reason) = job {
                                    ui.add(egui::Label::new(RichText::new(reason).color(theme::TEXT_MUTED)).truncate());
                                }
                            },
                            |ui| {
                                cancel = theme::dialog_button(ui, "Cancel").clicked();
                                ui.add_enabled_ui(job.is_ok(), |ui| {
                                    run = theme::primary_button(ui, "Run").clicked();
                                });
                            },
                        );
                    });
                egui::ScrollArea::vertical().id_salt("pg_tool_settings").show(ui, |ui| settings(ui, dialog));
            });
        let ready = job.is_ok();
        dialog.prepared = Some(prepared);
        if response.should_close() {
            cancel = true;
        }
        if cancel {
            self.dump_dialog = None;
            return;
        }
        // Built again so the input file is checked as the job starts.
        if run && ready && let Some(Ok(job)) = self.dump_dialog.as_ref().map(DumpDialog::job) {
            let dialog = self.dump_dialog.take().expect("open dialog");
            *self.config.pg_tools.get_mut(dialog.tool()) = dialog.tool_path.trim().to_string();
            let path = PathBuf::from(dialog.path.trim());
            self.config.transfer_dir = path.parent().map(Path::to_path_buf);
            let database = dialog.database();
            let (kind, title, target) = match dialog.mode {
                Mode::Dump { options, .. } => {
                    self.config.dump_options = options;
                    (TransferKind::Dump, format!("pg_dump {database}"), Some(path))
                }
                Mode::Restore { options, psql } => {
                    self.config.restore_options = options;
                    let tool = if psql { "psql" } else { "pg_restore" };
                    (TransferKind::Restore, format!("{tool} {database}"), None)
                }
            };
            self.save_config();
            let task = self.begin_transfer(kind, title, target, None);
            self.backend.run_tool(task, job);
            return;
        }
        self.resolve_tool();
    }
}

fn dump_path(dir: &Path, name: &str, format: DumpFormat) -> PathBuf {
    let stem = file_stem(name);
    match format.extension() {
        "" => dir.join(stem),
        extension => dir.join(format!("{stem}.{extension}")),
    }
}

fn settings(ui: &mut Ui, dialog: &mut DumpDialog) {
    let tool = dialog.tool();
    theme::section_header(ui, "Tool");
    theme::form_row(ui, &format!("Path to {}:", tool.binary()), LABEL_WIDTH, |ui| {
        let hint = dialog.resolved.as_ref().map_or_else(|| "Not found".into(), |p| p.display().to_string());
        ui.add(egui::TextEdit::singleline(&mut dialog.tool_path).hint_text(hint).desired_width((ui.available_width() - 96.0).max(140.0)));
        if theme::dialog_button(ui, "Browse…").clicked()
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            dialog.tool_path = path.to_string_lossy().into_owned();
        }
    });
    theme::form_row(ui, "", LABEL_WIDTH, |ui| match &dialog.version {
        Some(Ok(version)) if version.is_empty() => {
            ui.spinner();
        }
        Some(Ok(version)) => {
            ui.label(RichText::new(version.as_str()).small().color(theme::TEXT_MUTED));
        }
        Some(Err(error)) => {
            ui.label(RichText::new(error.as_str()).small().color(theme::RED));
        }
        None => {}
    });
    let database = dialog.database();
    let source = dialog.ds.display_name();
    match &mut dialog.mode {
        Mode::Dump { options, schemas, tables } => {
            theme::section_header(ui, "Objects");
            theme::form_row(ui, "Database:", LABEL_WIDTH, |ui| {
                ui.label(format!("{database} · {source}"));
            });
            theme::form_row(ui, "Schemas:", LABEL_WIDTH, |ui| {
                ui.add(egui::TextEdit::singleline(schemas).hint_text("All").desired_width(260.0));
                theme::help_icon(ui, "Comma-separated schema names. Empty dumps all schemas.");
            });
            theme::form_row(ui, "Tables:", LABEL_WIDTH, |ui| {
                ui.add(egui::TextEdit::singleline(tables).hint_text("All").desired_width(260.0));
                theme::help_icon(ui, "Comma-separated schema.table names. Empty dumps all tables of the chosen schemas.");
            });
            theme::section_header(ui, "Output");
            theme::form_row(ui, "Format:", LABEL_WIDTH, |ui| {
                let before = options.format;
                egui::ComboBox::from_id_salt("dump_format")
                    .selected_text(options.format.label())
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for format in DumpFormat::ALL {
                            ui.selectable_value(&mut options.format, format, format.label());
                        }
                    });
                if options.format != before {
                    let path = PathBuf::from(dialog.path.trim());
                    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    if let Some(parent) = path.parent() {
                        dialog.path = dump_path(parent, &stem, options.format).to_string_lossy().into_owned();
                    }
                }
            });
            let folder = options.format == DumpFormat::Directory;
            path_row(ui, &mut dialog.path, if folder { "Folder:" } else { "File:" }, folder, true);
            theme::section_header(ui, "Options");
            theme::form_row(ui, "Content:", LABEL_WIDTH, |ui| content_combo(ui, "dump_content", &mut options.content));
            theme::form_row(ui, "Statements:", LABEL_WIDTH, |ui| {
                egui::ComboBox::from_id_salt("dump_statements")
                    .selected_text(options.statements.label())
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for statements in Statements::ALL {
                            ui.selectable_value(&mut options.statements, statements, statements.label());
                        }
                    });
                if options.statements != Statements::Copy {
                    ui.label("Rows per INSERT:");
                    ui.add(egui::DragValue::new(&mut options.rows_per_insert).range(1..=1000));
                }
            });
            theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                ui.checkbox(&mut options.clean, "Clean (DROP before CREATE)");
                ui.add_enabled(options.clean, egui::Checkbox::new(&mut options.if_exists, "IF EXISTS"));
            });
            theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                ui.checkbox(&mut options.create, "Include CREATE DATABASE");
            });
            theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                ui.checkbox(&mut options.no_owner, "No owner");
                ui.checkbox(&mut options.no_privileges, "No privileges");
            });
            if options.format == DumpFormat::Directory {
                theme::form_row(ui, "Parallel jobs:", LABEL_WIDTH, |ui| {
                    ui.add(egui::DragValue::new(&mut options.jobs).range(1..=64));
                });
            }
            if matches!(options.format, DumpFormat::Custom | DumpFormat::Directory | DumpFormat::Plain) {
                theme::form_row(ui, "Compression:", LABEL_WIDTH, |ui| {
                    egui::ComboBox::from_id_salt("dump_compression")
                        .selected_text(options.compression.map_or_else(|| "Default".into(), |l| l.to_string()))
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut options.compression, None, "Default");
                            for level in 0..=9u8 {
                                ui.selectable_value(&mut options.compression, Some(level), level.to_string());
                            }
                        });
                });
            }
            theme::form_row(ui, "Other arguments:", LABEL_WIDTH, |ui| {
                ui.add(egui::TextEdit::singleline(&mut options.extra).hint_text("--exclude-table=… --lock-wait-timeout=…").desired_width(ui.available_width()));
            });
        }
        Mode::Restore { options, psql } => {
            theme::section_header(ui, "Target");
            theme::form_row(ui, "Database:", LABEL_WIDTH, |ui| {
                ui.label(format!("{database} · {source}"));
            });
            theme::section_header(ui, "Input");
            let psql = *psql;
            path_row(ui, &mut dialog.path, "File:", false, false);
            if !psql {
                theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                    ui.label(RichText::new("A custom or tar archive, or a directory dump").small().color(theme::TEXT_MUTED));
                    if ui.small_button("Choose Folder…").clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder()
                    {
                        dialog.path = path.to_string_lossy().into_owned();
                    }
                });
            }
            theme::section_header(ui, "Options");
            if !psql {
                theme::form_row(ui, "Content:", LABEL_WIDTH, |ui| content_combo(ui, "restore_content", &mut options.content));
                theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                    ui.checkbox(&mut options.clean, "Clean (DROP before CREATE)");
                    ui.add_enabled(options.clean, egui::Checkbox::new(&mut options.if_exists, "IF EXISTS"));
                });
                theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                    ui.checkbox(&mut options.create, "Create the database");
                });
                theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                    ui.checkbox(&mut options.no_owner, "No owner");
                    ui.checkbox(&mut options.no_privileges, "No privileges");
                });
            }
            theme::form_row(ui, "", LABEL_WIDTH, |ui| {
                ui.checkbox(&mut options.single_transaction, "Single transaction");
                ui.checkbox(&mut options.exit_on_error, "Stop at the first error");
            });
            if !psql {
                theme::form_row(ui, "Parallel jobs:", LABEL_WIDTH, |ui| {
                    ui.add_enabled(!options.single_transaction, egui::DragValue::new(&mut options.jobs).range(1..=64));
                });
            }
            theme::form_row(ui, "Other arguments:", LABEL_WIDTH, |ui| {
                ui.add(egui::TextEdit::singleline(&mut options.extra).desired_width(ui.available_width()));
            });
            if options.clean {
                ui.add_space(4.0);
                ui.label(RichText::new("Existing objects are dropped before they are restored.").color(theme::SEARCH_MARKER));
            }
        }
    }
}

fn content_combo(ui: &mut Ui, id: &str, content: &mut Content) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(content.label())
        .width(170.0)
        .show_ui(ui, |ui| {
            for option in Content::ALL {
                ui.selectable_value(content, option, option.label());
            }
        });
}

fn path_row(ui: &mut Ui, path: &mut String, label: &str, folder: bool, save: bool) {
    theme::form_row(ui, label, LABEL_WIDTH, |ui| {
        ui.add(egui::TextEdit::singleline(path).desired_width((ui.available_width() - 96.0).max(140.0)));
        if theme::dialog_button(ui, "Browse…").clicked() {
            let current = PathBuf::from(path.trim());
            let mut picker = rfd::FileDialog::new();
            if let Some(parent) = current.parent().filter(|p| p.is_dir()) {
                picker = picker.set_directory(parent);
            }
            let picked = match (folder, save) {
                (true, _) => picker.pick_folder(),
                (false, true) => {
                    if let Some(name) = current.file_name() {
                        picker = picker.set_file_name(name.to_string_lossy());
                    }
                    picker.save_file()
                }
                (false, false) => picker.pick_file(),
            };
            if let Some(picked) = picked {
                *path = picked.to_string_lossy().into_owned();
            }
        }
    });
}

/// The command exactly as it runs (without the password), with a Copy button.
fn command_preview(ui: &mut Ui, line: Option<&str>) {
    let Some(line) = line else {
        return;
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Command line").font(theme::semibold(13.0)));
        theme::help_icon(ui, "The password and SSL settings are passed through libpq environment variables, not arguments.");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            theme::flat_widgets(ui);
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(line.to_string());
            }
        });
    });
    egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(4).inner_margin(8).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Long paths break anywhere instead of splitting options at their dashes.
        let mut job = egui::text::LayoutJob::single_section(
            line.to_string(),
            egui::TextFormat::simple(egui::FontId::monospace(12.0), theme::TEXT_MUTED),
        );
        job.wrap = egui::text::TextWrapping { max_width: ui.available_width(), break_anywhere: true, ..Default::default() };
        ui.add(egui::Label::new(job).selectable(true));
    });
}
