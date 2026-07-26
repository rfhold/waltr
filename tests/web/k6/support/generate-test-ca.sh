#!/bin/sh
set -eu

public_dir=${TEST_CA_PUBLIC_DIR:-/test-ca-public}
caddy_dir=${TEST_CA_CADDY_DIR:-/test-ca-caddy}

validate_output_dir() {
  path=$1
  label=$2
  case "$path" in
    /*) ;;
    *) echo "$label must be an absolute directory: $path" >&2; exit 1 ;;
  esac
  if [ -L "$path" ] || [ ! -d "$path" ]; then
    echo "$label must be an existing non-symlink directory: $path" >&2
    exit 1
  fi
  physical_path=$(CDPATH= cd -P "$path" 2>/dev/null && pwd -P) || {
    echo "$label could not be resolved: $path" >&2
    exit 1
  }
  if [ "$physical_path" != "$path" ]; then
    echo "$label must not traverse symlinked parents: $path" >&2
    exit 1
  fi
}

validate_output_dir "$public_dir" "TEST_CA_PUBLIC_DIR"
validate_output_dir "$caddy_dir" "TEST_CA_CADDY_DIR"
if [ "$public_dir" = "$caddy_dir" ]; then
  echo "test CA output directories must be distinct" >&2
  exit 1
fi

work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

find "$public_dir" "$caddy_dir" -mindepth 1 -maxdepth 1 -exec rm -rf {} +

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -nodes \
  -keyout "$work_dir/ca.key" \
  -out "$work_dir/ca.crt" \
  -days 2 \
  -sha256 \
  -subj "/CN=Waltr integration test CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl req \
  -newkey rsa:2048 \
  -nodes \
  -keyout "$work_dir/tls.key" \
  -out "$work_dir/tls.csr" \
  -sha256 \
  -subj "/CN=waltr.test"

cat >"$work_dir/tls.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:waltr.test,DNS:dex.test
EOF

openssl x509 \
  -req \
  -in "$work_dir/tls.csr" \
  -CA "$work_dir/ca.crt" \
  -CAkey "$work_dir/ca.key" \
  -CAcreateserial \
  -out "$work_dir/tls.crt" \
  -days 2 \
  -sha256 \
  -extfile "$work_dir/tls.ext"

rm -f "$work_dir/ca.key"
cp "$work_dir/ca.crt" "$public_dir/ca.crt"
cp "$work_dir/tls.crt" "$caddy_dir/tls.crt"
cp "$work_dir/tls.key" "$caddy_dir/tls.key"
chmod 0444 "$public_dir/ca.crt" "$caddy_dir/tls.crt"
chmod 0400 "$caddy_dir/tls.key"
