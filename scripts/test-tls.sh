#!/usr/bin/env bash
# Disposable, loopback-only TLS fixtures. No existing containers or credentials are used.
# Run a custom command inside the fixture environment with: scripts/test-tls.sh command args...
set -euo pipefail
cd "$(dirname "$0")/.."
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/dbeast-tls.XXXXXX")
fixture_dir=$(cd "$fixture_dir" && pwd -P)
fixture_id="dbeast-tls-$$"
containers=()
cleanup() {
  for container in "${containers[@]}"; do docker rm -fv "$container" >/dev/null 2>&1 || true; done
  docker network rm "$fixture_id" >/dev/null 2>&1 || true
  rm -rf "$fixture_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
openssl_bin=${OPENSSL_BIN:-/usr/bin/openssl}
mkdir "$fixture_dir/certs" "$fixture_dir/config"
certs="$fixture_dir/certs"
# The enclosing temporary directory remains private; public mount files must be readable in Docker.
chmod 755 "$certs"
"$openssl_bin" req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=DBeast-Test-CA \
  -keyout "$certs/ca.key" -out "$certs/ca.pem" >/dev/null 2>&1
"$openssl_bin" req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=DBeast-Wrong-CA \
  -keyout "$certs/wrong-ca.key" -out "$certs/wrong-ca.pem" >/dev/null 2>&1
cat > "$certs/server.ext" <<'EXT'
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,DNS:pg,DNS:mysql
EXT
cat > "$certs/client.ext" <<'EXT'
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
EXT
for role in server client; do
  cn=localhost
  [ "$role" != client ] || cn=dbeast_mtls
  "$openssl_bin" req -newkey rsa:2048 -nodes -subj "/CN=$cn" \
    -keyout "$certs/$role.key" -out "$certs/$role.csr" >/dev/null 2>&1
  "$openssl_bin" x509 -req -in "$certs/$role.csr" -CA "$certs/ca.pem" -CAkey "$certs/ca.key" \
    -CAcreateserial -days 2 -extfile "$certs/$role.ext" -out "$certs/$role.pem" >/dev/null 2>&1
done
# Explicit legacy-compatible encryption works with Apple's PKCS#12 importer too.
"$openssl_bin" pkcs12 -export -inkey "$certs/client.key" -in "$certs/client.pem" \
  -certfile "$certs/ca.pem" -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -passout pass:fixture-only -out "$certs/client.p12" >/dev/null 2>&1
# PEM client keys in common formats: PKCS#8 (client.key), PKCS#1 RSA
# and an EC certificate with a SEC1 key.
"$openssl_bin" rsa -in "$certs/client.key" -traditional -out "$certs/client-rsa.key" >/dev/null 2>&1 || \
  "$openssl_bin" rsa -in "$certs/client.key" -out "$certs/client-rsa.key" >/dev/null 2>&1
"$openssl_bin" ecparam -name prime256v1 -genkey -noout -out "$certs/client-ec.key" >/dev/null 2>&1
"$openssl_bin" req -new -key "$certs/client-ec.key" -subj /CN=dbeast_mtls -out "$certs/client-ec.csr" >/dev/null 2>&1
"$openssl_bin" x509 -req -in "$certs/client-ec.csr" -CA "$certs/ca.pem" -CAkey "$certs/ca.key" \
  -CAcreateserial -days 2 -extfile "$certs/client.ext" -out "$certs/client-ec.pem" >/dev/null 2>&1
"$openssl_bin" x509 -in "$certs/ca.pem" -outform DER -out "$certs/ca.der"
cat "$certs/wrong-ca.pem" "$certs/ca.pem" > "$certs/ca-bundle.pem"
chmod 600 "$certs"/*.key "$certs/client.p12"
cat > "$certs/pg_hba.conf" <<'HBA'
local all all trust
hostssl all dbeast_mtls all scram-sha-256 clientcert=verify-full
host all all all scram-sha-256
HBA
chmod 644 "$certs"/*.pem "$certs/pg_hba.conf"
docker network create "$fixture_id" >/dev/null
for mode in tls plain; do
  pg_name="$fixture_id-pg-$mode"
  containers+=("$pg_name")
  pg_options=(-c ssl=off)
  if [ "$mode" = tls ]; then
    pg_options=(-c ssl=on -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key -c ssl_ca_file=/fixtures/ca.pem -c hba_file=/fixtures/pg_hba.conf)
  fi
  docker run -d --name "$pg_name" --network "$fixture_id" --network-alias "pg-$mode" \
    -p 127.0.0.1::5432 -e POSTGRES_USER=dbeast -e POSTGRES_PASSWORD=fixture-only -e POSTGRES_DB=dbeast \
    -v "$certs:/fixtures:ro" --entrypoint bash postgres:17 -c \
    'cp /fixtures/server.pem /tmp/server.pem; cp /fixtures/server.key /tmp/server.key; chown postgres:postgres /tmp/server.*; chmod 600 /tmp/server.key; exec docker-entrypoint.sh postgres "$@"' \
    -- "${pg_options[@]}" >/dev/null
  mysql_name="$fixture_id-mysql-$mode"
  containers+=("$mysql_name")
  mysql_options=(--tls-version=)
  if [ "$mode" = tls ]; then
    mysql_options=(--ssl-ca=/fixtures/ca.pem --ssl-cert=/tmp/server.pem --ssl-key=/tmp/server.key)
  fi
  docker run -d --name "$mysql_name" --network "$fixture_id" --network-alias "mysql-$mode" \
    -p 127.0.0.1::3306 -e MYSQL_ROOT_PASSWORD=fixture-only -e MYSQL_USER=dbeast -e MYSQL_PASSWORD=fixture-only -e MYSQL_DATABASE=dbeast \
    -v "$certs:/fixtures:ro" --entrypoint bash mysql:8.4 -c \
    'cp /fixtures/server.pem /tmp/server.pem; cp /fixtures/server.key /tmp/server.key; chown mysql:mysql /tmp/server.*; chmod 600 /tmp/server.key; exec docker-entrypoint.sh mysqld "$@"' \
    -- "${mysql_options[@]}" >/dev/null
done
# Certificate names match these database aliases, never the loopback tunnel endpoint.
docker network disconnect "$fixture_id" "$fixture_id-pg-tls"
docker network connect --alias pg --alias pg-tls "$fixture_id" "$fixture_id-pg-tls"
docker network disconnect "$fixture_id" "$fixture_id-mysql-tls"
docker network connect --alias mysql --alias mysql-tls "$fixture_id" "$fixture_id-mysql-tls"
docker build -q -t dbeast-tls-sshd tests/ssh >/dev/null
containers+=("$fixture_id-ssh")
docker run -d --name "$fixture_id-ssh" --network "$fixture_id" -p 127.0.0.1::22 dbeast-tls-sshd >/dev/null
for mode in tls plain; do
  ready=false
  for ((attempt=0; attempt<120; attempt++)); do
    if docker exec "$fixture_id-pg-$mode" pg_isready -h 127.0.0.1 -U dbeast >/dev/null 2>&1 && \
       docker exec "$fixture_id-mysql-$mode" mysql --get-server-public-key --protocol=tcp -h127.0.0.1 -uroot -pfixture-only -e 'SELECT 1' >/dev/null 2>&1; then
      ready=true; break
    fi
    sleep 1
  done
  if [ "$ready" != true ]; then echo "TLS fixture startup failed ($mode)" >&2; exit 1; fi
done
docker exec "$fixture_id-pg-tls" psql -U dbeast -d dbeast -c "CREATE ROLE dbeast_mtls LOGIN PASSWORD 'fixture-only'" >/dev/null
docker exec "$fixture_id-mysql-tls" mysql -uroot -pfixture-only -e \
  "CREATE USER 'dbeast_mtls'@'%' IDENTIFIED BY 'fixture-only' REQUIRE X509; GRANT ALL ON dbeast.* TO 'dbeast_mtls'@'%'" 2>/dev/null
port() { docker port "$1" "$2" | sed 's/^127\.0\.0\.1://'; }
export DBEAST_TLS_FIXTURES=1 DBEAST_TLS_CERTS="$certs" DBEAST_CONFIG="$fixture_dir/config/config.json"
export DBEAST_TLS_PG_PORT=$(port "$fixture_id-pg-tls" 5432)
export DBEAST_PLAIN_PG_PORT=$(port "$fixture_id-pg-plain" 5432)
export DBEAST_TLS_MYSQL_PORT=$(port "$fixture_id-mysql-tls" 3306)
export DBEAST_PLAIN_MYSQL_PORT=$(port "$fixture_id-mysql-plain" 3306)
export DBEAST_TLS_SSH_PORT=$(port "$fixture_id-ssh" 22)
if [ "$#" -gt 0 ]; then "$@"; else cargo test --test tls -- --ignored --test-threads=1 --nocapture; fi
