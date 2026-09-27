#!/bin/sh
# Makes a throwaway CA and one server certificate valid for both servers and
# for the per-app SQL proxy on 127.0.0.1. Nothing here is a real secret; the
# files live only in the run directory.
set -eu
out=/out
cd "${out}"
rm -f ./*.pem ./*.srl ./*.csr ./*.cnf
openssl req -x509 -newkey rsa:2048 -nodes -days 7 -subj "/CN=turso-e2e test CA" \
  -keyout ca-key.pem -out ca.pem 2>/dev/null
cat > server.cnf <<'EOF'
[req]
distinguished_name = dn
[dn]
[ext]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:turso, DNS:mysql, DNS:localhost, IP:127.0.0.1
EOF
openssl req -newkey rsa:2048 -nodes -subj "/CN=turso" \
  -keyout server-key-pkcs8.pem -out server.csr 2>/dev/null
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -days 7 -extfile server.cnf -extensions ext -out server.pem 2>/dev/null
cat server.pem ca.pem > server-chain.pem
mv server-key-pkcs8.pem server-key.pem
rm -f server.csr server.cnf ca.srl
chmod 0644 ./*.pem
