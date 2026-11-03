# Security notes

Known limits and accepted risks. Revisit each when its trigger or review date comes up.

## Tokens are not bound to a service

A token carries permission bits, not a service id. Whoever holds a token with a given
bit can use it on every service that offers that action. Accepted for now; binding
tokens to services is future work and needs a claim agreed with chulacraft-web.
Review date: 2027-02-02.

## The console WebSocket outlives its token

The token is checked when the console WebSocket is opened. The connection is not
closed when the token expires. Accepted for now; closing the socket at `exp` is future
work. Review date: 2027-02-02.

## `cargo audit`: rsa and crypto-bigint advisories

`cargo audit` reports advisories against `rsa` and `crypto-bigint`. They come in
through `jsonwebtoken`'s `rust_crypto` backend, but the manager only accepts Ed25519
(`Algorithm::EdDSA`, pinned in `Jwt::new`), so no RSA operation or code path using the
affected arithmetic is reachable. The advisories are not ignored in tooling; this is
the reasoning for leaving them open. Review date: 2027-02-02, or earlier if the
manager ever accepts another algorithm or the advisories gain an Ed25519-relevant
impact.

## Docker proxy access

The proxy socket is owned by group `mcsv-docker`, which holds only `mcsv-mgr`. The
proxy also checks the peer uid (SO_PEERCRED) and answers only root and the manager.
