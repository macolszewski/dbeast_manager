//! "Data Sources and Drivers" dialog.

use super::theme;
use crate::model::{DataSource, DriverKind, PasswordStorage, SshAuth, TlsMode};
use egui::{RichText, Ui};
use std::collections::HashMap;

enum TestState {
    Running,
    Done(Result<String, String>),
}

struct TestRecord {
    source: DataSource,
    request_id: String,
    state: TestState,
}

#[derive(Clone, Copy, PartialEq)]
enum FormTab {
    General,
    Options,
    Ssl,
}

pub struct DataSourceDialog {
    pub sources: Vec<DataSource>,
    pub selected: Option<usize>,
    tests: HashMap<String, TestRecord>,
    tab: FormTab,
    pub busy: bool,
    pub reload_discard: bool,
    pub error: Option<String>,
}

pub enum DialogAction {
    None,
    Test(Box<DataSource>, String),
    Apply,
    Ok,
    Cancel,
    Retry,
    ReloadSettings,
}

/// Preferred size; the dialog shrinks to fit smaller windows.
const PREFERRED: egui::Vec2 = egui::vec2(880.0, 600.0);
const MINIMUM: egui::Vec2 = egui::vec2(520.0, 320.0);
const LABEL_WIDTH: f32 = 150.0;

impl DataSourceDialog {
    pub fn new(sources: Vec<DataSource>, select_id: Option<&str>) -> Self {
        let selected = select_id
            .and_then(|id| sources.iter().position(|s| s.id == id))
            .or((!sources.is_empty()).then_some(0));
        Self {
            sources,
            selected,
            tests: HashMap::new(),
            tab: FormTab::General,
            busy: false,
            reload_discard: false,
            error: None,
        }
    }

    pub fn add(&mut self, driver: DriverKind) {
        let mut ds = DataSource::new(driver);
        if driver == DriverKind::Postgres {
            ds.user = "postgres".into();
            ds.database = "postgres".into();
        }
        self.sources.push(ds);
        self.selected = Some(self.sources.len() - 1);
        self.tab = FormTab::General;
    }

    pub fn show(&mut self, ctx: &egui::Context) -> DialogAction {
        let mut action = DialogAction::None;
        // Modal, movable and resizable.
        let response = theme::Dialog::new(
            egui::Id::new("data_sources_dialog"),
            RichText::new("Data Sources and Drivers").font(theme::semibold(14.0)),
        )
        .default_size(PREFERRED)
        .min_size(MINIMUM)
        .frame(egui::Frame::window(&ctx.global_style()).inner_margin(0))
        .show(ctx, |ui| {
                if self.busy {
                    ui.disable();
                }
                ui.set_min_size(ui.available_size());
                egui::Panel::bottom("ds_footer")
                    .resizable(false)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 10)))
                    .show(ui, |ui| {
                        if self.busy {
                            ui.label(if self.reload_discard { "Reloading saved settings…" } else { "Saving passwords and settings…" });
                        }
                        if let Some(error) = &self.error {
                            ui.label(RichText::new(error).color(theme::RED));
                            ui.label("Unsaved data source changes will be discarded only after a successful reload.");
                            if ui.button("Reload settings and discard changes").clicked() {
                                action = DialogAction::ReloadSettings;
                            }
                        }
                        if self.sources.iter().any(|s| s.credentials_unavailable) {
                            ui.label("Saved passwords could not be loaded.");
                            if ui.button("Retry password loading").clicked() {
                                action = DialogAction::Retry;
                            }
                        }
                        ui.horizontal(|ui| {
                            if let Some((ds, request_id)) = self.test_row(ui) {
                                action = DialogAction::Test(Box::new(ds), request_id);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if theme::dialog_button(ui, "Apply").clicked() {
                                        action = DialogAction::Apply;
                                    }
                                    if theme::dialog_button(ui, "Cancel").clicked() {
                                        action = DialogAction::Cancel;
                                    }
                                    if theme::primary_button(ui, "OK").clicked() {
                                        action = DialogAction::Ok;
                                    }
                                },
                            );
                        });
                    });
                egui::Panel::left("ds_list")
                    .resizable(true)
                    .default_size(230.0)
                    .size_range(160.0..=420.0)
                    .frame(
                        egui::Frame::new()
                            .fill(theme::BG_EDITOR)
                            .inner_margin(egui::Margin::symmetric(8, 6)),
                    )
                    .show(ui, |ui| self.source_list(ui));
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 10)))
                    .show(ui, |ui| {
                        match self.selected.filter(|&i| i < self.sources.len()) {
                            Some(i) => self.form(ui, i),
                            None => {
                                ui.add_space(40.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        RichText::new("No data sources").color(theme::TEXT_MUTED),
                                    );
                                    ui.label(
                                        RichText::new("Add one with + on the left")
                                            .small()
                                            .color(theme::TEXT_MUTED),
                                    );
                                });
                            }
                        }
                    });
            });
        if !self.busy && response.should_close() {
            action = DialogAction::Cancel;
        }
        action
    }

    fn source_list(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Data Sources").font(theme::semibold(12.5)));
            ui.label(RichText::new("Drivers").color(theme::TEXT_MUTED))
                .on_hover_text("PostgreSQL, MySQL and SQLite drivers are built in");
        });
        ui.horizontal(|ui| {
            theme::tool_menu(ui, RichText::new("+").size(15.0), "Add data source", |ui| {
                for d in DriverKind::ALL {
                    if ui
                        .button(format!("{}  {}", super::driver_badge(d), d.label()))
                        .clicked()
                    {
                        self.add(d);
                        ui.close();
                    }
                }
            });
            let has = self.selected.is_some_and(|i| i < self.sources.len());
            if theme::tool_button_enabled(ui, has, "−", "Remove").clicked()
                && let Some(i) = self.selected
            {
                self.sources.remove(i);
                self.selected = if self.sources.is_empty() {
                    None
                } else {
                    Some(i.saturating_sub(1))
                };
            }
            if theme::tool_button_enabled(ui, has, "📋", "Duplicate").clicked()
                && let Some(i) = self.selected
            {
                let mut copy = self.sources[i].clone();
                copy.id = uuid::Uuid::new_v4().to_string();
                copy.credential_ref = None;
                copy.name = format!("{} (copy)", copy.display_name());
                self.sources.push(copy);
                self.selected = Some(self.sources.len() - 1);
            }
        });
        theme::hairline(ui);
        ui.add_space(4.0);
        ui.label(
            RichText::new("Project Data Sources")
                .small()
                .color(theme::TEXT_MUTED),
        );
        egui::ScrollArea::vertical()
            .id_salt("ds_list_scroll")
            .show(ui, |ui| {
                for (i, ds) in self.sources.iter().enumerate() {
                    let text = format!("{}  {}", super::driver_badge(ds.driver), ds.display_name());
                    let selected = self.selected == Some(i);
                    let resp = ui
                        .with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                            ui.add(
                                egui::Button::selectable(selected, text)
                                    .min_size(egui::vec2(0.0, 24.0)),
                            )
                        })
                        .inner;
                    if resp.clicked() {
                        self.selected = Some(i);
                    }
                }
            });
    }

    fn form(&mut self, ui: &mut Ui, i: usize) {
        let ds = &mut self.sources[i];
        field_row(ui, "Name:", |ui| {
            let hint = ds.display_name();
            ui.add(
                egui::TextEdit::singleline(&mut ds.name)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY),
            );
        });
        field_row(ui, "Driver:", |ui| {
            let before = ds.driver;
            egui::ComboBox::from_id_salt("driver")
                .selected_text(format!(
                    "{}  {}",
                    super::driver_badge(ds.driver),
                    ds.driver.label()
                ))
                .width(200.0)
                .show_ui(ui, |ui| {
                    for d in DriverKind::ALL {
                        ui.selectable_value(&mut ds.driver, d, d.label());
                    }
                });
            if before != ds.driver {
                ds.port = ds.driver.default_port();
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (tab, label) in [
                (FormTab::General, "General"),
                (FormTab::Options, "Options"),
                (FormTab::Ssl, "SSH/SSL"),
            ] {
                if theme::flat_tab(ui, RichText::new(label), self.tab == tab, false)
                    .0
                    .clicked()
                {
                    self.tab = tab;
                }
            }
        });
        theme::hairline(ui);
        ui.add_space(8.0);

        egui::ScrollArea::vertical().id_salt("ds_form").auto_shrink([false, false]).show(ui, |ui| match self.tab {
            FormTab::General => general_tab(ui, ds),
            FormTab::Options => {
                field_row_help(ui, "Access:", "Opens sessions in read-only transaction mode; writes are rejected by the server.", |ui| {
                    ui.checkbox(&mut ds.read_only, "Read-only");
                });
            }
            FormTab::Ssl => {
                if ds.driver.is_file_based() {
                    hint_row(ui, "SQLite opens a local file, there is no network connection to secure.");
                } else {
                    ssh_section(ui, ds);
                    ui.add_space(14.0);
                    section_title(ui, "SSL");
                    field_row(ui, "", |ui| {
                        ui.checkbox(&mut ds.use_tls, "Use SSL");
                    });
                    ui.add_enabled_ui(ds.use_tls, |ui| {
                        let mode_help = match ds.tls.mode {
                            TlsMode::Require => "Encrypts the connection without checking the server certificate. Use it only on trusted networks.",
                            TlsMode::VerifyCa => "Encrypts and checks that the server certificate is signed by a trusted CA; the host name is not checked.",
                            TlsMode::VerifyFull => "Encrypts and checks the server certificate and that it is issued for the host name, also through SSH.",
                        };
                        field_row_help(ui, "Mode:", mode_help, |ui| {
                            egui::ComboBox::from_id_salt("tls_mode")
                                .selected_text(ds.tls.mode.label())
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for mode in TlsMode::ALL {
                                        ui.selectable_value(&mut ds.tls.mode, mode, mode.label());
                                    }
                                });
                        });
                        file_field_help(ui, "CA file:", &mut ds.tls.ca_file, "optional", Some(("Certificates", &["pem", "crt", "cer", "der"])), home, Some("PEM or DER CA certificates, in addition to the system's trusted roots."));
                        file_field_help(ui, "Client certificate:", &mut ds.tls.identity_file, "optional", Some(("Certificates", &["pem", "crt", "cer", "der", "p12", "pfx"])), home, Some("A PEM certificate (chain) with the key file below, or a PKCS#12 (.p12/.pfx) archive with both and no key file."));
                        file_field_help(ui, "Client key file:", &mut ds.tls.key_file, "empty for a PKCS#12 certificate", Some(("Private keys", &["pem", "key", "der"])), home, Some("Unencrypted PEM private key: PKCS#8, PKCS#1 RSA or SEC1 EC."));
                        tls_field(ui, "PKCS#12 password:", &mut ds.tls_identity_password, true);

                    });
                }
            }
        });
    }

    pub fn finish_test(&mut self, source_id: &str, request_id: &str, result: Result<String, String>) {
        if let Some(record) = self.tests.get_mut(source_id)
            && record.request_id == request_id
            && self.sources.contains(&record.source)
        {
            record.state = TestState::Done(result);
        }
    }

    /// "Test Connection" link with its result, shown in the dialog footer.
    fn test_row(&mut self, ui: &mut Ui) -> Option<(DataSource, String)> {
        let ds = self.selected.and_then(|i| self.sources.get(i))?;
        let mut test = None;
        if ui
            .add_enabled(
                !ds.credentials_unavailable,
                egui::Link::new("Test Connection"),
            )
            .clicked()
        {
            test = Some((ds.clone(), uuid::Uuid::new_v4().to_string()));
        }
        if self.tests.get(&ds.id).is_some_and(|record| record.source != *ds) {
            self.tests.remove(&ds.id);
        }
        match self.tests.get(&ds.id).map(|record| &record.state) {
            Some(TestState::Running) => {
                ui.spinner();
            }
            Some(TestState::Done(Ok(info))) => {
                ui.label(RichText::new(format!("✔ Succeeded · {info}")).color(theme::GREEN));
            }
            Some(TestState::Done(Err(e))) => {
                let first = e.lines().next().unwrap_or_default();
                ui.label(RichText::new(format!("✖ {first}")).color(theme::RED))
                    .on_hover_text(e);
            }
            None => {}
        }
        if let Some((ds, request_id)) = &test {
            self.tests.insert(ds.id.clone(), TestRecord { source: ds.clone(), request_id: request_id.clone(), state: TestState::Running });
        }
        test
    }
}

fn general_tab(ui: &mut Ui, ds: &mut DataSource) {
    if ds.driver.is_file_based() {
        file_field(
            ui,
            "File:",
            &mut ds.file,
            "/path/to/database.sqlite",
            Some(("SQLite database", &["sqlite", "sqlite3", "db", "db3", "gpkg"])),
            home,
        );
    } else {
        field_row(ui, "Host:", |ui| {
            let port_w = 90.0;
            let host_w = (ui.available_width() - port_w - 50.0).max(120.0);
            ui.add(egui::TextEdit::singleline(&mut ds.host).desired_width(host_w));
            ui.label("Port:");
            ui.add_sized(
                [port_w, 24.0],
                egui::DragValue::new(&mut ds.port).range(1..=65535),
            );
        });
        field_row(ui, "Authentication:", |ui| {
            ui.label("User & Password");
        });
        field_row(ui, "User:", |ui| {
            ui.add(egui::TextEdit::singleline(&mut ds.user).desired_width(260.0));
        });
        field_row(ui, "Password:", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut ds.password)
                    .password(true)
                    .desired_width(260.0),
            );
        });
        storage_row(ui, ds);
        field_row(ui, "Database:", |ui| {
            ui.add(egui::TextEdit::singleline(&mut ds.database).desired_width(f32::INFINITY));
        });
    }
    field_row(ui, "URL:", |ui| {
        theme::help_icon(ui, &format!("Built-in driver: native Rust {} client", driver_crate(ds.driver)));
        ui.add(
            egui::Label::new(RichText::new(ds.url()).monospace().color(theme::TEXT_MUTED))
                .truncate(),
        );
    });
    if ds.uses_ssh() {
        hint_row(
            ui,
            &format!(
                "Connects through the SSH tunnel {} (SSH/SSL tab); host and port above are resolved on that server.",
                ds.ssh_label()
            ),
        );
    }
}

fn section_title(ui: &mut Ui, title: &str) {
    theme::section_header(ui, title);
}

/// "Use SSH tunnel" with the server, user and authentication, on the SSH/SSL tab.
fn ssh_section(ui: &mut Ui, ds: &mut DataSource) {
    section_title(ui, "SSH");
    field_row(ui, "", |ui| {
        ui.checkbox(&mut ds.ssh.enabled, "Use SSH tunnel");
        theme::help_icon(ui, "SSH passwords and the key passphrase follow the Save setting on the General tab.");
    });
    let ssh = &mut ds.ssh;
    ui.add_enabled_ui(ssh.enabled, |ui| {
        field_row(ui, "Host:", |ui| {
            let port_w = 90.0;
            let host_w = (ui.available_width() - port_w - 50.0).max(120.0);
            ui.add(egui::TextEdit::singleline(&mut ssh.host).hint_text("bastion.example.com").desired_width(host_w));
            ui.label("Port:");
            ui.add_sized([port_w, 24.0], egui::DragValue::new(&mut ssh.port).range(1..=65535));
        });
        field_row(ui, "User name:", |ui| {
            ui.add(egui::TextEdit::singleline(&mut ssh.user).desired_width(260.0));
        });
        field_row(ui, "Authentication:", |ui| {
            egui::ComboBox::from_id_salt("ssh_auth")
                .selected_text(ssh.auth.label())
                .width(200.0)
                .show_ui(ui, |ui| {
                    for a in SshAuth::ALL {
                        ui.selectable_value(&mut ssh.auth, a, a.label());
                    }
                });
        });
        match ssh.auth {
            SshAuth::Password => field_row(ui, "Password:", |ui| {
                ui.add(egui::TextEdit::singleline(&mut ssh.password).password(true).desired_width(260.0));
            }),
            SshAuth::KeyPair => {
                file_field(
                    ui,
                    "Private key file:",
                    &mut ssh.key_file,
                    "~/.ssh/id_ed25519",
                    None,
                    || home().map(|home| home.join(".ssh")),
                );
                field_row(ui, "Passphrase:", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ssh.passphrase)
                            .password(true)
                            .hint_text("empty if the key has none")
                            .desired_width(260.0),
                    );
                });
            }
            SshAuth::Agent => field_row(ui, "", |ui| {
                ui.label(RichText::new("Keys from the running ssh-agent (SSH_AUTH_SOCK)").color(theme::TEXT_MUTED));
            }),
        }
        hint_row(
            ui,
            "The host key is remembered on first connection; a different key later blocks the connection.",
        );
    });
}

/// Label column of fixed width followed by the field, as in IDE settings forms.
fn field_row(ui: &mut Ui, label: &str, field: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_WIDTH, 24.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(LABEL_WIDTH);
                ui.label(label);
            },
        );
        field(ui);
    });
    ui.add_space(4.0);
}

/// A form row with an ⓘ after the field carrying its explanation, so the form
/// shows fields, not paragraphs.
fn field_row_help(ui: &mut Ui, label: &str, help: &str, field: impl FnOnce(&mut Ui)) {
    field_row(ui, label, |ui| {
        field(ui);
        theme::help_icon(ui, help);
    });
}

fn hint_row(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_WIDTH + 6.0);
        ui.add(egui::Label::new(RichText::new(text).small().color(theme::TEXT_MUTED)).wrap());
    });
}

fn driver_crate(d: DriverKind) -> &'static str {
    match d {
        DriverKind::Postgres => "tokio-postgres",
        DriverKind::MySql => "mysql_async",
        DriverKind::Sqlite => "rusqlite (bundled SQLite)",
    }
}

fn storage_row(ui: &mut Ui, ds: &mut DataSource) {
    let help = if cfg!(target_os = "macos") {
        "Applies to database, SSH and TLS identity passwords. Forever keeps them in the macOS Keychain, Until restart only in memory."
    } else if cfg!(target_os = "linux") {
        "Applies to database, SSH and TLS identity passwords. Forever keeps them in the system keyring (Secret Service), Until restart only in memory."
    } else {
        "System password storage is unavailable on this platform. Choose Until restart."
    };
    field_row_help(ui, "Save:", help, |ui| {
        egui::ComboBox::from_id_salt("password_storage")
            .selected_text(match ds.password_storage {
                PasswordStorage::Forever => "Forever",
                PasswordStorage::ForSession => "Until restart",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut ds.password_storage,
                    PasswordStorage::Forever,
                    "Forever",
                );
                ui.selectable_value(
                    &mut ds.password_storage,
                    PasswordStorage::ForSession,
                    "Until restart",
                );
            });
    });
    if ds.credentials_unavailable {
        field_row(ui, "", |ui| {
            let mut replace = false;
            if ui
                .checkbox(&mut replace, "Replace unavailable passwords")
                .changed()
                && replace
            {
                ds.credentials_unavailable = false;
                ds.password.clear();
                ds.ssh.password.clear();
                ds.ssh.passphrase.clear();
                ds.tls_identity_password.clear();
            }
        });
        hint_row(
            ui,
            "Replacing saves the passwords you enter here, including empty fields.",
        );
    }
}

fn tls_field(ui: &mut Ui, label: &str, value: &mut String, password: bool) {
    ui.horizontal(|ui| {
        let label = ui.allocate_ui_with_layout(
            egui::vec2(LABEL_WIDTH, 24.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| { ui.set_min_width(LABEL_WIDTH); ui.label(label) },
        ).inner;
        // Passwords have the width of the other password fields.
        ui.add(egui::TextEdit::singleline(value).password(password).desired_width(if password { 260.0 } else { f32::INFINITY }))
            .labelled_by(label.id);
        if password {
            theme::help_icon(ui, &format!("Follows the Save setting on the General tab ({} or Until restart).", crate::credentials::STORE_NAME));
        }
    });
    ui.add_space(4.0);
}

/// A path field with Browse… opening the system file chooser in the field's
/// folder (or `fallback`); the path can still be typed or pasted.
fn file_field(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    filter: Option<(&str, &[&str])>,
    fallback: impl FnOnce() -> Option<std::path::PathBuf>,
) {
    file_field_help(ui, label, value, hint, filter, fallback, None);
}

fn file_field_help(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    filter: Option<(&str, &[&str])>,
    // Resolved only when Browse… is clicked, not on every frame.
    fallback: impl FnOnce() -> Option<std::path::PathBuf>,
    help: Option<&str>,
) {
    let name = label;
    ui.horizontal(|ui| {
        let label = ui
            .allocate_ui_with_layout(
                egui::vec2(LABEL_WIDTH, 24.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(LABEL_WIDTH);
                    ui.label(label)
                },
            )
            .inner;
        let width = (ui.available_width() - if help.is_some() { 108.0 } else { 86.0 }).max(140.0);
        ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(width))
            .labelled_by(label.id);
        if theme::dialog_button(ui, "Browse…")
            .on_hover_ui(|ui| {
                ui.label(format!("Choose the {} in a file dialog", name.trim_end_matches(':').to_lowercase()));
            })
            .clicked()
        {
            let mut dialog = rfd::FileDialog::new();
            let current = std::path::Path::new(value.as_str());
            let folder = current
                .parent()
                .filter(|dir| !dir.as_os_str().is_empty() && dir.is_dir())
                .map(std::path::Path::to_path_buf)
                .or_else(|| fallback().filter(|dir| dir.is_dir()));
            if let Some(folder) = folder {
                dialog = dialog.set_directory(folder);
            }
            if let Some((name, extensions)) = filter {
                dialog = dialog.add_filter(name, extensions).add_filter("All files", &["*"]);
            }
            if let Some(path) = dialog.pick_file() {
                *value = path.to_string_lossy().into_owned();
            }
        }
        if let Some(help) = help {
            theme::help_icon(ui, help);
        }
    });
    ui.add_space(4.0);
}

fn home() -> Option<std::path::PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

#[cfg(test)]
mod tls_test_result_tests {
    use super::*;

    #[test]
    fn changed_tls_settings_and_old_requests_cannot_publish_success() {
        let mut source = DataSource::new(DriverKind::Postgres);
        source.use_tls = true;
        let id = source.id.clone();
        let mut dialog = DataSourceDialog::new(vec![source.clone()], None);
        dialog.tests.insert(id.clone(), TestRecord {
            source: source.clone(), request_id: "new".into(), state: TestState::Running,
        });
        dialog.finish_test(&id, "old", Ok("TLS (verified)".into()));
        assert!(matches!(dialog.tests[&id].state, TestState::Running));
        dialog.sources[0].tls.ca_file = "different-ca.pem".into();
        dialog.finish_test(&id, "new", Ok("TLS (verified)".into()));
        assert!(matches!(dialog.tests[&id].state, TestState::Running));
        dialog.sources[0] = source;
        dialog.finish_test(&id, "new", Ok("TLS (verified)".into()));
        assert!(matches!(dialog.tests[&id].state, TestState::Done(Ok(_))));
        dialog.sources[0].tls_identity_password = "changed-synthetic-secret".into();
        dialog.finish_test(&id, "new", Err("obsolete error".into()));
        assert!(matches!(dialog.tests[&id].state, TestState::Done(Ok(_))));
    }
}
