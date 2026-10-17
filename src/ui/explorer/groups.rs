//! Data source groups and order in Database Explorer.
//!
//! Groups are explorer layout only: `Config::source_groups` keeps their order
//! and `Config::source_group` maps a source ID to its group path. Source order
//! is the order of `Config::data_sources`.

use super::navigation::GROUP_KEY;
use crate::ui::App;

/// Where a dragged data source is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) enum SourceDrop {
    /// Before or after another source, joining its group.
    Beside { target: String, after: bool },
    /// At the end of a group (empty path: the top level).
    Into(String),
}

/// Pending New Group / Rename Group dialog.
pub(in crate::ui) struct GroupDialog {
    pub mode: GroupDialogMode,
    pub name: String,
    pub error: Option<&'static str>,
    pub focus: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) enum GroupDialogMode {
    /// New group under `parent`, optionally moving a source into it.
    New {
        parent: String,
        source: Option<String>,
    },
    Rename(String),
}

pub(in crate::ui) fn segments(path: &str) -> Vec<String> {
    path.split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Trims each segment; rejects empty names and control characters.
pub(in crate::ui) fn normalize(path: &str) -> Result<String, &'static str> {
    if path.chars().any(char::is_control) {
        return Err("A group name cannot contain control characters.");
    }
    let parts = segments(path);
    if parts.is_empty() {
        return Err("Enter a group name.");
    }
    if path.split('/').any(|segment| segment.trim().is_empty()) {
        return Err("Group names cannot be empty between \"/\" separators.");
    }
    Ok(parts.join("/"))
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn within(path: &str, group: &str) -> bool {
    path == group || path.starts_with(&format!("{group}/"))
}

impl App {
    /// Every group path in display order, parents before their children.
    pub(in crate::ui) fn explorer_groups(&self) -> Vec<Vec<String>> {
        let mut ordered: Vec<String> = Vec::new();
        let add = |path: &str, ordered: &mut Vec<String>| {
            let parts = segments(path);
            for end in 1..=parts.len() {
                let prefix = parts[..end].join("/");
                if !ordered.contains(&prefix) {
                    ordered.push(prefix);
                }
            }
        };
        for group in &self.config.source_groups {
            add(group, &mut ordered);
        }
        for source in &self.config.data_sources {
            if let Some(group) = self.config.source_group.get(&source.id) {
                add(group, &mut ordered);
            }
        }
        ordered.iter().map(|path| segments(path)).collect()
    }

    pub(in crate::ui) fn source_group_of(&self, source_id: &str) -> &str {
        self.config
            .source_group
            .get(source_id)
            .map_or("", String::as_str)
    }

    fn groups_changed(&mut self) {
        // Remove stale memberships of deleted sources and empty group entries.
        let ids: std::collections::HashSet<&str> = self
            .config
            .data_sources
            .iter()
            .map(|source| source.id.as_str())
            .collect();
        self.config
            .source_group
            .retain(|id, group| ids.contains(id.as_str()) && !group.is_empty());
        let mut seen = std::collections::HashSet::new();
        self.config
            .source_groups
            .retain(|group| !group.is_empty() && seen.insert(group.clone()));
        self.explorer_ui.invalidate();
        self.save_config();
    }

    pub(in crate::ui) fn create_group(&mut self, parent: &str, name: &str, source: Option<&str>) -> Result<(), &'static str> {
        let path = normalize(&join(parent, name))?;
        if !self.config.source_groups.contains(&path) {
            self.config.source_groups.push(path.clone());
        }
        let mut open = Vec::new();
        for part in segments(&path) {
            open.push(part);
            self.expanded.insert((GROUP_KEY.into(), open.clone()));
        }
        if let Some(source) = source {
            self.config.source_group.insert(source.into(), path);
        }
        self.groups_changed();
        Ok(())
    }

    pub(in crate::ui) fn rename_group(&mut self, old: &str, name: &str) -> Result<(), &'static str> {
        let parent = segments(old);
        let parent = parent[..parent.len().saturating_sub(1)].join("/");
        let new = normalize(&join(&parent, name))?;
        if new == old {
            return Ok(());
        }
        if self.explorer_groups().iter().any(|group| group.join("/") == new) {
            return Err("A group with this name already exists.");
        }
        let rename = |path: &str| {
            if within(path, old) {
                format!("{new}{}", &path[old.len()..])
            } else {
                path.to_owned()
            }
        };
        for group in &mut self.config.source_groups {
            *group = rename(group);
        }
        for group in self.config.source_group.values_mut() {
            *group = rename(group);
        }
        let old_parts = segments(old);
        let new_parts = segments(&new);
        let expanded: Vec<_> = self
            .expanded
            .iter()
            .filter(|(id, path)| id == GROUP_KEY && path.starts_with(&old_parts))
            .cloned()
            .collect();
        for (id, path) in expanded {
            self.expanded.remove(&(id.clone(), path.clone()));
            let mut renamed = new_parts.clone();
            renamed.extend_from_slice(&path[old_parts.len()..]);
            self.expanded.insert((id, renamed));
        }
        self.selected = Some((GROUP_KEY.into(), new_parts));
        self.groups_changed();
        Ok(())
    }

    /// Removes a group; its sources and subgroups move to its parent.
    pub(in crate::ui) fn remove_group(&mut self, path: &str) {
        let parts = segments(path);
        let parent = parts[..parts.len().saturating_sub(1)].join("/");
        let lift = |group: &str| -> String {
            if group == path {
                parent.clone()
            } else if within(group, path) {
                join(&parent, &group[path.len() + 1..])
            } else {
                group.to_owned()
            }
        };
        for group in &mut self.config.source_groups {
            *group = lift(group);
        }
        for group in self.config.source_group.values_mut() {
            *group = lift(group);
        }
        self.expanded
            .retain(|(id, expanded)| id != GROUP_KEY || !expanded.starts_with(&parts));
        if self
            .selected
            .as_ref()
            .is_some_and(|(id, selected)| id == GROUP_KEY && selected.starts_with(&parts))
        {
            self.selected = None;
        }
        self.groups_changed();
    }

    pub(in crate::ui) fn move_source_to_group(&mut self, source: &str, group: &str) {
        if group.is_empty() {
            self.config.source_group.remove(source);
        } else {
            self.config.source_group.insert(source.into(), group.into());
            let mut open = Vec::new();
            for part in segments(group) {
                open.push(part);
                self.expanded.insert((GROUP_KEY.into(), open.clone()));
            }
        }
        self.groups_changed();
    }

    /// Drag and drop: reorders `config.data_sources` and adopts the target group.
    pub(in crate::ui) fn drop_source(&mut self, source: &str, target: SourceDrop) {
        let Some(from) = self.config.data_sources.iter().position(|s| s.id == source) else {
            return;
        };
        match target {
            SourceDrop::Beside { target, after } => {
                if target == source {
                    return;
                }
                let group = self.source_group_of(&target).to_owned();
                let moved = self.config.data_sources.remove(from);
                let Some(index) = self.config.data_sources.iter().position(|s| s.id == target)
                else {
                    self.config.data_sources.insert(from, moved);
                    return;
                };
                self.config
                    .data_sources
                    .insert(index + usize::from(after), moved);
                self.move_source_to_group(source, &group);
            }
            SourceDrop::Into(group) => {
                // Append after the last source already in that group.
                let moved = self.config.data_sources.remove(from);
                let index = self
                    .config
                    .data_sources
                    .iter()
                    .rposition(|s| self.config.source_group.get(&s.id).map_or("", String::as_str) == group)
                    .map_or(self.config.data_sources.len(), |last| last + 1);
                self.config.data_sources.insert(index, moved);
                self.move_source_to_group(source, &group);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataSource, DriverKind};

    fn app() -> App {
        let ctx = egui::Context::default();
        let mut app = crate::ui::actions::tests::isolated_app(&ctx);
        for name in ["alpha", "beta", "gamma"] {
            let mut source = DataSource::new(DriverKind::Sqlite);
            source.id = name.into();
            source.name = name.into();
            app.config.data_sources.push(source);
        }
        app
    }

    fn order(app: &App) -> Vec<&str> {
        app.config.data_sources.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn names_are_normalized_and_validated() {
        assert_eq!(normalize(" Clients / Acme ").unwrap(), "Clients/Acme");
        assert!(normalize("").is_err());
        assert!(normalize("a//b").is_err());
        assert!(normalize("bad\nname").is_err());
    }

    #[test]
    fn groups_nest_rename_and_lift_without_touching_sources() {
        let mut app = app();
        let before = app.config.data_sources.clone();
        app.create_group("", "Clients", Some("alpha")).unwrap();
        app.create_group("Clients", "Acme", Some("beta")).unwrap();
        assert_eq!(
            app.explorer_groups(),
            [vec!["Clients".to_string()], vec!["Clients".into(), "Acme".into()]]
        );
        app.rename_group("Clients", "Projects").unwrap();
        assert_eq!(app.source_group_of("alpha"), "Projects");
        assert_eq!(app.source_group_of("beta"), "Projects/Acme");
        assert!(app.expanded.contains(&(GROUP_KEY.into(), vec!["Projects".into(), "Acme".into()])));
        assert!(app.rename_group("Projects/Acme", "Acme").is_ok());
        app.create_group("", "Other", None).unwrap();
        assert_eq!(app.rename_group("Other", "Projects"), Err("A group with this name already exists."));
        app.remove_group("Projects");
        assert_eq!(app.source_group_of("alpha"), "");
        assert_eq!(app.source_group_of("beta"), "Acme");
        // Grouping is layout only: connection settings stay identical.
        assert!(app.config.data_sources == before);
    }

    #[test]
    fn dragging_reorders_and_joins_the_target_group() {
        let mut app = app();
        app.create_group("", "Work", Some("gamma")).unwrap();
        app.drop_source("alpha", SourceDrop::Beside { target: "gamma".into(), after: true });
        assert_eq!(order(&app), ["beta", "gamma", "alpha"]);
        assert_eq!(app.source_group_of("alpha"), "Work");
        app.drop_source("beta", SourceDrop::Into("Work".into()));
        assert_eq!(order(&app), ["gamma", "alpha", "beta"]);
        assert_eq!(app.source_group_of("beta"), "Work");
        app.drop_source("alpha", SourceDrop::Beside { target: "gamma".into(), after: false });
        assert_eq!(order(&app), ["alpha", "gamma", "beta"]);
        app.drop_source("gamma", SourceDrop::Into(String::new()));
        assert_eq!(app.source_group_of("gamma"), "");
        assert_eq!(*order(&app).last().unwrap(), "gamma");
    }
}
