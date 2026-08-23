#!/usr/bin/env bash
# Create the "infra" intermediate and the leaf that blocky serves for DoH,
# either under an existing root or under a fresh one made for this box alone.
#
#   make-doh-cert.sh --new-root [outdir]
#   make-doh-cert.sh <root-ca.crt> <root-ca.key> [outdir]
#
# A dedicated root is the default for a reason: this box is carried into
# hostile networks, so the anchor that vouches for it should vouch for nothing
# else. Chaining it to an estate-wide or employer CA widens the blast radius
# of a stolen router for no benefit.
#
# Long-lived on purpose. This box goes to hotels; it cannot reach a CA to
# renew, and a DoH listener whose certificate expired mid-trip is a router
# with no DNS. The policy that makes that acceptable is the same one that
# says: if a device doesn't trust this CA, don't use TLS to it.
set -euo pipefail

ROOT_DAYS=3650
INT_DAYS=3650
LEAF_DAYS=3650

NEW_ROOT=0
if [[ "${1:-}" == "--new-root" ]]; then
  NEW_ROOT=1
  OUT="${2:-./doh-pki}"
else
  ROOT_CRT="${1:?usage: make-doh-cert.sh [--new-root] | <root-ca.crt> <root-ca.key> [outdir]}"
  ROOT_KEY="${2:?need the root CA private key}"
  OUT="${3:-./doh-pki}"
  # Resolved before the cd below, or a relative path to the root stops
  # resolving the moment we move into the output directory.
  ROOT_CRT=$(readlink -f "$ROOT_CRT")
  ROOT_KEY=$(readlink -f "$ROOT_KEY")
fi

# Both listen addresses, plus a name. IP SANs are the load-bearing ones: a
# client asking THIS box to resolve names cannot resolve the box's own name
# first, so DoH has to work when configured as https://10.9.141.1/dns-query.
SANS="IP:10.9.141.1,IP:10.6.141.1,IP:127.0.0.1,DNS:rasputin"

mkdir -p "$OUT"; cd "$OUT"
umask 077

if [[ $NEW_ROOT == 1 ]]; then
  echo "==> root: Rasputin Root CA (offline - keep this key off the router)"
  openssl ecparam -name prime256v1 -genkey -noout -out root-ca.key
  # pathlen:1 - root signs the intermediate, the intermediate signs leaves,
  # and the chain stops there.
  openssl req -x509 -new -key root-ca.key -out root-ca.crt -days "$ROOT_DAYS" -sha256 \
    -subj "/O=Rasputin/CN=Rasputin Root CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:1" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -addext "subjectKeyIdentifier=hash"
  ROOT_CRT="$PWD/root-ca.crt"
  ROOT_KEY="$PWD/root-ca.key"
fi

echo "==> intermediate: Rasputin Infra Intermediate CA"
openssl ecparam -name prime256v1 -genkey -noout -out infra-ca.key
openssl req -new -key infra-ca.key -out infra-ca.csr \
  -subj "/O=Rasputin/CN=Rasputin Infra Intermediate CA"

# pathlen:0 - this intermediate signs leaves and nothing else. Without it the
# intermediate could mint further CAs, which is how one stolen key becomes a
# trust-anchor compromise.
cat > infra-ca.ext <<EXT
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
EXT

openssl x509 -req -in infra-ca.csr -CA "$ROOT_CRT" -CAkey "$ROOT_KEY" \
  -CAcreateserial -out infra-ca.crt -days "$INT_DAYS" -sha256 -extfile infra-ca.ext

echo "==> leaf: rasputin DoH"
openssl ecparam -name prime256v1 -genkey -noout -out rasputin.key
openssl req -new -key rasputin.key -out rasputin.csr \
  -subj "/O=Rasputin/CN=rasputin"

cat > rasputin.ext <<EXT
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = $SANS
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
EXT

openssl x509 -req -in rasputin.csr -CA infra-ca.crt -CAkey infra-ca.key \
  -CAcreateserial -out rasputin.crt -days "$LEAF_DAYS" -sha256 -extfile rasputin.ext

# blocky serves exactly what is in certFile, and sends no intermediate of its
# own accord. Without the chain here, every client that does not already hold
# the intermediate fails verification.
cat rasputin.crt infra-ca.crt > rasputin-fullchain.crt

echo "==> verify against the root"
openssl verify -CAfile "$ROOT_CRT" -untrusted infra-ca.crt rasputin.crt

echo
echo "Wrote in $PWD:"
if [[ $NEW_ROOT == 1 ]]; then
echo "  root-ca.crt             INSTALL THIS on every device that uses the AP"
echo "  root-ca.key             KEEP OFFLINE - never goes near the router"
fi
echo "  infra-ca.crt            the intermediate (safe to publish)"
echo "  infra-ca.key            KEEP OFFLINE - not needed on the router"
echo "  rasputin-fullchain.crt  -> blocky_doh_cert"
echo "  rasputin.key            -> blocky_doh_key"
