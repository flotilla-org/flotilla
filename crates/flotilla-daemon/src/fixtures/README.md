# GitHub App test keys

`github_app_test.pem` and `github_app_test.pub.pem` are a throwaway RSA test key pair, not GitHub App credentials. They are intentionally committed for offline request-contract tests. Never use them for live authentication or replace them with a real App key.

The public key is derived with `openssl pkey -in github_app_test.pem -pubout`.
