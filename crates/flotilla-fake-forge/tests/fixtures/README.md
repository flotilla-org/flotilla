# Test TLS fixtures

`ca.pem` is a test-only certificate authority. `server.pem` is a server
certificate it issued for `localhost` and `127.0.0.1`, with `server-key.pem`
as its key. The TLS round-trip test serves `server.pem` and trusts only
`ca.pem`. Both certificates are valid until 2126. The CA key was discarded
after signing, so nothing else can be issued from this CA.

Generated with OpenSSL 3.0.13:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout ca-key.pem -out ca.pem -days 36500 \
  -subj "/CN=flotilla-fake-forge test CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout server-key.pem -out server.csr -subj "/CN=localhost"
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -out server.pem -days 36500 -extfile server.ext
rm -f server.csr ca.srl server.ext ca-key.pem
```
