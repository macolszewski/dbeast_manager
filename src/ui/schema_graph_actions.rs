//! Opening Database Graph from Database Explorer and carrying out its intents.

use super::schema_graph::{GraphIntent, layout_key};
use super::{App, ServicesNotice};
use crate::model::{DbObject, DriverKind, ObjectKind};

impl App {
    /// Resolves the database (and schemas) a graph covers from an explorer node.
    pub(super) fn open_database_graph(&mut self, ds_id: &str, node: Option<&DbObject>) {
        let Some(source) = self.source(ds_id).cloned() else {
            return;
        };
        let roots = self.sources.get(ds_id).and_then(|state| state.roots.as_ref());
        let scope = match (source.driver, node) {
            (DriverKind::Postgres, Some(node)) if node.kind == ObjectKind::Schema => node
                .path
                .first()
                .map(|database| (database.clone(), Some(vec![node.name.clone()]))),
            (DriverKind::Postgres, Some(node)) if node.kind == ObjectKind::Database => {
                // Schemas hidden in Databases and Schemas stay out of the scan when known.
                let schemas = node.children.as_ref().map(|children| {
                    children
                        .iter()
                        .filter(|child| child.kind == ObjectKind::Schema && source.shows_schema(&node.name, &child.name))
                        .map(|child| child.name.clone())
                        .collect::<Vec<_>>()
                });
                Some((node.name.clone(), schemas))
            }
            (DriverKind::Postgres, _) => {
                let opened = roots.and_then(|roots| {
                    roots
                        .iter()
                        .find(|root| root.kind == ObjectKind::Database && root.detail == crate::drivers::DEFAULT_DATABASE)
                        .map(|root| root.name.clone())
                });
                (!source.database.is_empty())
                    .then(|| source.database.clone())
                    .or(opened)
                    .map(|database| (database, None))
            }
            (DriverKind::MySql, Some(node)) if node.kind == ObjectKind::Schema => Some((node.name.clone(), None)),
            (DriverKind::MySql, _) => (!source.database.is_empty()).then(|| (source.database.clone(), None)),
            (DriverKind::Sqlite, Some(node)) if node.kind == ObjectKind::Schema && node.name != "main" => {
                self.notice = Some(ServicesNotice::info("Database Graph reads the main database of a SQLite file".into()));
                return;
            }
            (DriverKind::Sqlite, _) => Some(("main".into(), None)),
        };
        let Some((database, schemas)) = scope else {
            self.notice = Some(ServicesNotice::info(
                "Choose a database in Database Explorer to show its graph".into(),
            ));
            return;
        };
        let mut title = format!("{} · {database}", source.name);
        if let Some([schema]) = schemas.as_deref() {
            title.push_str(&format!(".{schema}"));
        }
        let saved = self
            .graph_layouts
            .get(&layout_key(ds_id, &database))
            .cloned()
            .unwrap_or_default();
        let intent = self
            .schema_graph
            .open(ds_id.to_owned(), title, database, schemas, saved);
        self.dispatch_graph(intent);
    }

    pub(super) fn dispatch_graph(&mut self, intent: GraphIntent) {
        match intent {
            GraphIntent::Load {
                ds_id,
                database,
                schemas,
                request,
                request_id,
            } => match self.source(&ds_id).cloned() {
                Some(source) => self
                    .backend
                    .load_schema_graph(source, database, schemas, request, request_id),
                None => self
                    .schema_graph
                    .loaded(request_id, Err("The data source no longer exists".into())),
            },
            GraphIntent::Relayout {
                graph,
                flat,
                pins,
                params,
                request_id,
            } => self.backend.layout_schema_graph(graph, flat, pins, params, request_id),
            GraphIntent::OpenData { ds_id, node } => self.open_table(ds_id, &node),
            GraphIntent::Ddl { ds_id, node } => {
                if let Some(ds) = self.source(&ds_id).cloned() {
                    let ds = match super::explorer::pg_database(&ds, &node) {
                        Some(database) => ds.with_database(&database),
                        None => ds,
                    };
                    self.backend.generate_ddl(ds, node);
                }
            }
            GraphIntent::Cancel => self.backend.cancel_schema_graph(),
            GraphIntent::SaveLayout { key, layout } => {
                self.graph_layouts.insert(key, layout);
            }
            GraphIntent::ResetLayout { key } => {
                self.graph_layouts.remove(&key);
            }
        }
    }
}
