//! Explorer capture and source guards for read-only DDL comparison.

use super::{
    App, ServicesNotice,
    ddl_diff::{StructureCompareSource, StructureIntent},
};
use crate::{
    backend::{DdlObjectRequest, StructureCompareRequest},
    ddl_diff::{ComparedDdl, DdlObjectIdentity},
    model::{DataSource, DbObject, DriverKind, ObjectKind},
};

fn identity(source: &DataSource, node: &DbObject) -> DdlObjectIdentity {
    let mut captured = source.identity();
    if source.driver == DriverKind::Postgres
        && let Some(database) = node.path.first()
    {
        captured.database.clone_from(database);
    }
    DdlObjectIdentity {
        ds_id: source.id.clone(),
        source: captured,
        kind: node.kind,
        path: node.path.clone(),
    }
}

fn supported(source: &DataSource, node: &DbObject) -> Option<String> {
    if !matches!(node.kind, ObjectKind::Table | ObjectKind::View) {
        return Some("Choose a table or view".into());
    }
    if source.driver == DriverKind::Sqlite
        && (node.path.first().is_none_or(|schema| schema != "main")
            || source.file.is_empty()
            || source.file == ":memory:")
    {
        return Some("Compare Structure With requires a persistent SQLite main database".into());
    }
    None
}

impl App {
    pub(super) fn open_structure_compare(&mut self, ds_id: &str, node: &DbObject) {
        let Some(source) = self.source(ds_id) else {
            return;
        };
        if let Some(reason) = supported(source, node) {
            self.notice = Some(ServicesNotice::info(reason));
            return;
        }
        let origin = identity(source, node);
        let sources = self.capture_structure_sources(&origin);
        if !sources.iter().any(|source| source.identity == origin) {
            self.notice = Some(ServicesNotice::info(
                "Reload the object in Database Explorer before comparing its structure".into(),
            ));
            return;
        }
        self.pending_editor_focus = false;
        self.pending_action_focus = None;
        self.structure_compare.open(sources, origin);
    }

    /// Only already fetched objects are traversed, once per explicit opening.
    fn capture_structure_sources(&self, origin: &DdlObjectIdentity) -> Vec<StructureCompareSource> {
        fn visit(
            app: &App,
            source: &DataSource,
            nodes: &[DbObject],
            origin: &DdlObjectIdentity,
            result: &mut Vec<StructureCompareSource>,
        ) {
            for node in nodes {
                if !super::explorer::root_visible(source, node) {
                    continue;
                }
                if source.driver == DriverKind::Postgres
                    && node.kind == ObjectKind::Schema
                    && !source
                        .shows_schema(node.path.first().map_or("", String::as_str), &node.name)
                {
                    continue;
                }
                if node.kind == origin.kind {
                    let captured = identity(source, node);
                    let path = node
                        .path
                        .iter()
                        .filter(|name| !name.starts_with('\0'))
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(".");
                    let target = &captured.source;
                    let location = if source.driver == DriverKind::Sqlite {
                        format!("SQLite {} · {path}", target.file)
                    } else {
                        let ssh = target
                            .ssh
                            .as_ref()
                            .map(|ssh| format!(" via {}@{}:{}", ssh.user, ssh.host, ssh.port))
                            .unwrap_or_default();
                        format!(
                            "{}@{}:{} / {}{ssh} · {path}",
                            target.user, target.host, target.port, target.database
                        )
                    };
                    result.push(StructureCompareSource {
                        identity: captured,
                        label: format!("{} / {path} [{}]", source.display_name(), source.id),
                        location,
                        blocked: supported(source, node).or_else(|| {
                            app.source_is_saving(&source.id)
                                .then(|| "Wait for the data source file to finish saving".into())
                        }),
                    });
                }
                if let Some(children) = &node.children {
                    visit(app, source, children, origin, result);
                }
            }
        }
        let mut result = Vec::new();
        for source in self
            .config
            .data_sources
            .iter()
            .chain(self.files.iter().map(|file| &file.source))
        {
            if source.driver != origin.source.driver {
                continue;
            }
            if let Some(roots) = self
                .sources
                .get(&source.id)
                .and_then(|state| state.roots.as_deref())
            {
                visit(self, source, roots, origin, &mut result);
            }
        }
        result
    }

    fn structure_source(&self, captured: &DdlObjectIdentity) -> Result<DataSource, String> {
        let mut source = self
            .source(&captured.ds_id)
            .cloned()
            .ok_or("The data source is no longer available. Reopen Compare Structure With.")?;
        if source.driver == DriverKind::Postgres {
            let database = captured
                .path
                .first()
                .ok_or("The object has no database path")?;
            source = source.with_database(database);
        }
        if source.identity() != captured.source {
            return Err("The data source changed. Reopen Compare Structure With.".into());
        }
        if self.source_is_saving(&captured.ds_id) {
            return Err("Wait for the data source file to finish saving".into());
        }
        Ok(source)
    }

    pub(super) fn dispatch_structure_compare(&mut self, intent: StructureIntent) {
        let StructureIntent::Compare {
            request_id,
            origin,
            target,
            cancelled,
        } = intent;
        let request = |captured: DdlObjectIdentity| -> Result<DdlObjectRequest, String> {
            let ds = self.structure_source(&captured)?;
            let name = captured
                .path
                .last()
                .cloned()
                .ok_or("The object has no name")?;
            let node = DbObject {
                name,
                kind: captured.kind,
                path: captured.path.clone(),
                detail: String::new(),
                children: None,
                expandable: false,
            };
            if let Some(reason) = supported(&ds, &node) {
                return Err(reason);
            }
            Ok(DdlObjectRequest {
                ds,
                identity: captured,
                node,
            })
        };
        match request(origin).and_then(|origin| request(target).map(|target| (origin, target))) {
            Ok((origin, target)) => self.backend.compare_structure(StructureCompareRequest {
                request_id,
                origin,
                target,
                cancelled,
            }),
            Err(reason) => self.structure_compare.done(request_id, Err(reason)),
        }
    }

    pub(super) fn structure_compared(
        &mut self,
        request_id: u64,
        result: Result<ComparedDdl, String>,
    ) {
        let Some((origin, target)) = self.structure_compare.pending_identities(request_id) else {
            return;
        };
        let guard = self
            .structure_source(origin)
            .and_then(|_| self.structure_source(target));
        let result = match guard {
            Err(reason) => Err(reason),
            Ok(_) => result.and_then(|diff| {
                if &diff.origin.identity == origin && &diff.target.identity == target {
                    Ok(diff)
                } else {
                    Err(
                        "The comparison returned different objects. Reopen Compare Structure With."
                            .into(),
                    )
                }
            }),
        };
        self.structure_compare.done(request_id, result);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SourceState, actions::tests::isolated_app};
    use super::*;

    fn schema(source: &DataSource, database: &str, name: &str) -> DbObject {
        let parent = if source.driver == DriverKind::Postgres {
            vec![database.into()]
        } else {
            Vec::new()
        };
        let mut schema = DbObject::new(ObjectKind::Schema, name, &parent);
        schema.children = Some(vec![
            DbObject::new(ObjectKind::Table, "same", &schema.path),
            DbObject::new(ObjectKind::View, "view", &schema.path),
        ]);
        schema
    }
    #[test]
    fn capture_uses_loaded_collapsed_objects_and_respects_scope() {
        let mut app = isolated_app(&egui::Context::default());
        let mut ds = DataSource::new(DriverKind::Postgres);
        ds.database = "one".into();
        ds.scope
            .hidden_schemas
            .insert("one".into(), vec!["hidden".into()]);
        let visible = schema(&ds, "one", "visible");
        let origin = identity(&ds, &visible.children.as_ref().unwrap()[0]);
        let mut database = DbObject::new(ObjectKind::Database, "one", &[])
            .with_detail(crate::drivers::DEFAULT_DATABASE);
        database.children = Some(vec![visible, schema(&ds, "one", "hidden")]);
        app.sources.insert(
            ds.id.clone(),
            SourceState {
                roots: Some(vec![database]),
                ..Default::default()
            },
        );
        app.config.data_sources.push(ds);
        assert!(app.expanded.is_empty());
        let snapshots = app.capture_structure_sources(&origin);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].identity, origin);
        assert!(!snapshots[0].label.contains('\0'));
        assert!(app.sources.values().all(|state| state.loading.is_empty()));
    }
    #[test]
    fn physical_database_and_source_changes_are_guarded() {
        let mut app = isolated_app(&egui::Context::default());
        let mut ds = DataSource::new(DriverKind::Postgres);
        ds.database = "one".into();
        let table = DbObject::new(ObjectKind::Table, "same", &["two".into(), "public".into()]);
        let origin = identity(&ds, &table);
        app.config.data_sources.push(ds);
        assert_eq!(app.structure_source(&origin).unwrap().database, "two");
        app.config.data_sources[0].name = "renamed".into();
        assert!(app.structure_source(&origin).is_ok());
        app.config.data_sources[0].host = "replacement".into();
        assert!(app.structure_source(&origin).is_err());
        app.config.data_sources.clear();
        assert!(app.structure_source(&origin).is_err());
    }
    #[test]
    fn sqlite_attached_and_memory_objects_are_not_offered_as_main() {
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.file = "/existing.db".into();
        let mut table = DbObject::new(ObjectKind::Table, "same", &["main".into()]);
        assert!(supported(&ds, &table).is_none());
        table.path[0] = "attached".into();
        assert!(supported(&ds, &table).is_some());
        table.path[0] = "main".into();
        ds.file = ":memory:".into();
        assert!(supported(&ds, &table).is_some());
    }
}
