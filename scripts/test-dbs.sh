#!/usr/bin/env bash
# Starts PostgreSQL and MySQL test containers and loads the sample "shop" schema.
set -euo pipefail
cd "$(dirname "$0")/.."

docker start dbeast-pg 2>/dev/null || docker run -d --name dbeast-pg -e POSTGRES_USER=dbeast -e POSTGRES_PASSWORD=dbeast -e POSTGRES_DB=dbeast -p 55432:5432 postgres:17
# PostGIS for the Geo Viewer; the official image has no arm64 build, this one is multi-arch.
docker start dbeast-postgis 2>/dev/null || docker run -d --name dbeast-postgis -e POSTGRES_USER=dbeast -e POSTGRES_PASSWORD=dbeast -e POSTGRES_DB=gis -p 55433:5432 imresamu/postgis:17-3.5
docker start dbeast-mysql 2>/dev/null || docker run -d --name dbeast-mysql -e MYSQL_ROOT_PASSWORD=dbeast -e MYSQL_DATABASE=dbeast -e MYSQL_USER=dbeast -e MYSQL_PASSWORD=dbeast -p 53306:3306 mysql:8.4

# SSH bastion for tunnel tests; the databases are reachable from it as "pg" and "mysql".
docker network inspect dbeast-net >/dev/null 2>&1 || docker network create dbeast-net >/dev/null
docker network connect --alias pg dbeast-net dbeast-pg 2>/dev/null || true
docker network connect --alias mysql dbeast-net dbeast-mysql 2>/dev/null || true
mkdir -p tests/ssh/keys
[ -f tests/ssh/keys/id_ed25519 ] || ssh-keygen -q -t ed25519 -N "" -C dbeast-test -f tests/ssh/keys/id_ed25519
[ -f tests/ssh/keys/id_protected ] || ssh-keygen -q -t ed25519 -N "secret" -C dbeast-test-protected -f tests/ssh/keys/id_protected
cat tests/ssh/keys/id_ed25519.pub tests/ssh/keys/id_protected.pub > tests/ssh/keys/tunnel
docker build -q -t dbeast-sshd tests/ssh >/dev/null
docker start dbeast-ssh 2>/dev/null || docker run -d --name dbeast-ssh --network dbeast-net -p 52222:22 \
  -v "$PWD/tests/ssh/keys/tunnel:/etc/ssh/authorized_keys/tunnel:ro" dbeast-sshd >/dev/null

# Over TCP: the temporary server of the first start listens only on the socket.
until docker exec dbeast-pg pg_isready -h 127.0.0.1 -U dbeast >/dev/null 2>&1; do sleep 1; done
until docker exec dbeast-postgis pg_isready -h 127.0.0.1 -U dbeast >/dev/null 2>&1; do sleep 1; done
until docker exec dbeast-mysql mysql -h127.0.0.1 -uroot -pdbeast -e "select 1" >/dev/null 2>&1; do sleep 1; done

docker exec -i dbeast-pg psql -q -U dbeast -d dbeast < tests/seed_pg.sql
docker exec dbeast-pg psql -q -U dbeast -d postgres -c "drop database if exists dbeast_archive with (force)" -c "create database dbeast_archive"
docker exec -i dbeast-pg psql -q -U dbeast -d dbeast_archive < tests/seed_pg_archive.sql
docker exec -i dbeast-postgis psql -q -U dbeast -d gis < tests/seed_postgis.sql 2>&1 | grep -v NOTICE || true
docker exec -i dbeast-mysql mysql --default-character-set=utf8mb4 -uroot -pdbeast < tests/seed_mysql.sql 2>/dev/null
docker exec dbeast-mysql mysql -uroot -pdbeast -e "grant all on shop.* to 'dbeast'@'%'" 2>/dev/null
echo "PostgreSQL: localhost:55432 dbeast/dbeast, MySQL: 127.0.0.1:53306 dbeast/dbeast"
echo "PostGIS: localhost:55433 dbeast/dbeast, database gis, schema geo"
echo "SSH: tunnel@localhost:52222 (password tunnel, keys in tests/ssh/keys), databases inside: pg:5432, mysql:3306"
