# KMS client

`quorum_rs::kms`: how a service asks the key manager (noolog-node `kms/`) for
its secrets, and the wire both sides share.

```rust
let kms = quorum_rs::kms::KmsClient::new("http://kms:8443", &seed)?;
let token = kms.derive("internal/orch-to-node/v1").await?;
```

| Call | Route | Answer |
|---|---|---|
| `derive(label)` | `POST /derive {label}` | `hex(HMAC-SHA256(root, label))` |
| `account_public_key()` | `GET /account` | the account's public key (`A…`) |
| `user_jwt(&UserJwt)` | `POST /user-jwt` | a NATS user JWT signed by the account |

Every request carries `x-kms-key` (the service's nkey public key),
`x-kms-nonce` (`<unix millis>.<random>`, used once) and `x-kms-signature`
(hex nkey signature over `canonical(nonce, method, path, body)` =
`<nonce>:<METHOD>:<path>:<hex sha256(body)>`). The KMS refuses an unknown key,
a nonce more than 60 s from now or already used, and a label outside the
service's prefixes; a refusal comes back as an error naming the status.

The KMS listens on the internal network only.
