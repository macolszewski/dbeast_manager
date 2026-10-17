-- Geo Viewer test data: points in WGS84 and polygons in PL-1992 (EPSG:2180).
create extension if not exists postgis;
drop schema if exists geo cascade;
create schema geo;
create table geo.cities (
    id serial primary key,
    name text not null,
    location geometry(Point, 4326),
    location_pl geometry(Point, 2180),
    area geography(Polygon, 4326)
);
insert into geo.cities (name, location) values
    ('Warszawa', 'SRID=4326;POINT(21.0122 52.2297)'),
    ('Kraków', 'SRID=4326;POINT(19.9450 50.0647)'),
    ('Gdańsk', 'SRID=4326;POINT(18.6466 54.3520)'),
    ('Bydgoszcz', 'SRID=4326;POINT(18.0084 53.1235)'),
    ('Wrocław', 'SRID=4326;POINT(17.0385 51.1079)'),
    ('No location', null);
update geo.cities set location_pl = st_transform(location, 2180),
    area = st_buffer(location::geography, 5000)::geometry::geography
where location is not null;

create table geo.parcels (
    id serial primary key,
    number text not null,
    shape geometry(Polygon, 2180)
);
-- Square parcels around Bydgoszcz, built in PL-1992 metres.
insert into geo.parcels (number, shape)
select 'B/' || g,
       st_makeenvelope(x, y, x + 150, y + 100, 2180)
from (select g, 473000 + (g % 5) * 200 as x, 583000 + (g / 5) * 150 as y
      from generate_series(0, 14) g) s;
analyze;
