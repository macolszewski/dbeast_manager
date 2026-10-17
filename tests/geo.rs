//! Geometries read from PostGIS and MySQL decode and land on the right spot
//! after reprojection to WGS84.

use dbeast::drivers::Connection;
use dbeast::geo::{Shape, build_layer, geometry_columns};
use dbeast::model::{DataSource, DriverKind, Page};

async fn open(driver: DriverKind, port: u16, db: &str) -> Option<Connection> {
    let mut ds = DataSource::new(driver);
    ds.host = "127.0.0.1".into();
    (ds.port, ds.user, ds.password, ds.database) =
        (port, "dbeast".into(), "dbeast".into(), db.into());
    Connection::open(&ds)
        .await
        .map_err(|e| eprintln!("skipping: {e}"))
        .ok()
}

fn first_point(shapes: &[Shape]) -> [f64; 2] {
    match &shapes[0] {
        Shape::Point(p) => *p,
        Shape::Polygon(r) => r[0][0],
        Shape::Line(l) => l[0],
    }
}

#[tokio::test]
async fn postgis_columns_srids_and_reprojection() {
    let Some(mut conn) = open(DriverKind::Postgres, 55433, "gis").await else {
        return;
    };
    let r = conn
        .execute(
            "select id, name, location, location_pl, area from geo.cities order by id",
            Page::first(100),
        )
        .await
        .unwrap();
    assert_eq!(
        geometry_columns(&r.columns, &r.rows),
        [2, 3, 4],
        "point, projected point and geography"
    );

    let wgs = build_layer(&r.rows, 2, None);
    let pl = build_layer(&r.rows, 3, None);
    assert_eq!(
        (wgs.srid, pl.srid, pl.srid_source),
        (Some(4326), Some(2180), "from data")
    );
    assert_eq!(
        (pl.features.len(), pl.skipped),
        (5, 1),
        "the city without location is skipped"
    );
    // PL-1992 back to WGS84 matches the original coordinates (PROJ in PostGIS vs proj4rs).
    for (a, b) in wgs.features.iter().zip(&pl.features) {
        let (p, q) = (first_point(&a.shapes), first_point(&b.shapes));
        assert!(
            (p[0] - q[0]).abs() < 1e-6 && (p[1] - q[1]).abs() < 1e-6,
            "{p:?} vs {q:?}"
        );
    }
    let geog = build_layer(&r.rows, 4, None);
    assert!(matches!(&geog.features[0].shapes[0], Shape::Polygon(rings) if rings[0].len() > 8));

    let parcels = conn
        .execute("select number, shape from geo.parcels", Page::first(100))
        .await
        .unwrap();
    let layer = build_layer(&parcels.rows, 1, None);
    assert_eq!(layer.features.len(), 15);
    let corner = first_point(&layer.features[0].shapes);
    assert!(
        (18.0..19.2).contains(&corner[0]) && (52.9..53.4).contains(&corner[1]),
        "{corner:?}"
    );

    // Text output of PostGIS functions is recognized too.
    let text = conn
        .execute("select st_astext(location_pl) as wkt, st_asgeojson(location) as json, location::text as hex from geo.cities where id = 1", Page::first(1))
        .await
        .unwrap();
    assert_eq!(geometry_columns(&text.columns, &text.rows), [0, 1, 2]);
    assert!(build_layer(&text.rows, 0, Some(2180)).error.is_none());
}

#[tokio::test]
async fn mysql_spatial_values() {
    let Some(mut conn) = open(DriverKind::MySql, 53306, "shop").await else {
        return;
    };
    let r = conn
        .execute(
            "select 'Warszawa' as name, ST_GeomFromText('POINT(637382.2 486757.2)', 2180) as g, ST_GeomFromText('POLYGON((0 0, 1 0, 1 1, 0 0))') as plain",
            Page::first(1),
        )
        .await
        .unwrap();
    assert_eq!(geometry_columns(&r.columns, &r.rows), [1, 2]);
    let layer = build_layer(&r.rows, 1, None);
    assert_eq!(layer.srid, Some(2180));
    let p = first_point(&layer.features[0].shapes);
    assert!(
        (p[0] - 21.0122).abs() < 1e-4 && (p[1] - 52.2297).abs() < 1e-4,
        "{p:?}"
    );
}
