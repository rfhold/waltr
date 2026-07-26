#!/bin/sh
set -eu

ca_file=/test-ca-public/ca.crt
if [ -f "$ca_file" ]; then
  nss_dir="$HOME/.pki/nssdb"
  mkdir -p "$nss_dir"
  certutil -N --empty-password -d "sql:$nss_dir"
  certutil -A -d "sql:$nss_dir" -n "Waltr integration test CA" -t "C,," -i "$ca_file"
  certutil -L -d "sql:$nss_dir" -n "Waltr integration test CA" >/dev/null
fi

exec /usr/bin/k6 "$@"
