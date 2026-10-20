#!/bin/bash
# Container-only setup. No host configuration, production credentials, or system install.
set -euo pipefail
mkdir -p /tmp/envoy-spike
cd /tmp/envoy-spike
curl -fL https://github.com/envoyproxy/envoy/releases/download/v1.40.0/envoy-1.40.0-linux-x86_64 -o envoy
chmod +x envoy
uv venv venv
uv pip install --python venv/bin/python -r /workspace/flotilla/docs/research/3593-envoy-credential-proxy/requirements.txt
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca-key.pem -out ca.pem -days 2 -subj /CN=Envoy-Spike-Test-CA
openssl req -newkey rsa:2048 -nodes -keyout key.pem -out leaf.csr -subj /CN=forge.test
printf 'subjectAltName=DNS:forge.test,DNS:git.test\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > leaf.ext
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out cert.pem -days 2 -extfile leaf.ext
printf '#!/bin/sh\nprintf "username=user\\npassword=placeholder-a\\n"\n' > credential-helper
chmod +x credential-helper
git init --bare repo.git
git --git-dir=repo.git config http.receivepack true
