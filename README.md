# Minecraft Server Manager

A web-based [Minecraft](https://www.minecraft.net/) server manager integrated into [Apache Server](https://httpd.apache.org/) for [Linux](https://kernel.org/) with [Systemd](https://github.com/systemd/systemd).

## Installation

Build the project
```bash
cargo build --release
```

Run both as root, in either order (`install.sh` creates the `mcsv-mgr` group itself
if `setup.sh` has not run yet). Run `setup.sh` once per host; it never rewrites
existing files under `/srv/minecraft`.

Installation
- install the binaries, the Docker proxy and the backup script and units
```bash
sudo bash scripts/install.sh
```

Basic setup
- setup user & group for safety
- install apache config file, systemd service and socket, the `minecraft@.service` template and the polkit rule
```bash
sudo bash scripts/setup.sh
```

Configuration
- copy [config/mcsv_manager/config.example.toml](config/mcsv_manager/config.example.toml)
  to `/etc/mcsv_manager/config.toml` and edit it (services and `[backups]`).
  Keep the file mode 0644: it holds no secrets, and the docker proxy runs as a dynamic
  user (group `mcsv-docker`) that must be able to read it; on a permission error it
  names the file and exits.
  Retention and credentials for the backup script live in
  `/etc/chulacraft/backup/backup.env` (`KEEP_DAILY`, `KEEP_WEEKLY`), not here.

Auth: the socket is mode 0660, group `mcsv-http`, so only members can connect
(`setup.sh` creates the group and adds `www-data`, which Apache runs as, and
`mcsv-mgr`; restart Apache afterwards). Add any operator who runs `mcsvctl` or
`mcsv-console`, since both default to this socket. Being able to connect is not
authority. A request passes if either
- it carries a short-lived EdDSA token signed by chulacraft-web
  (`Authorization: Bearer <jwt>`; the console WebSocket sends it as the second
  subprotocol, `mcsv.jwt, <jwt>`). A valid token always decides the request,
  whoever connected, or
- the connecting process is root or has a uid listed in `MCSV_TRUSTED_UIDS`
  (checked with the kernel's peer credentials, so the CLI tools in `src/bin`
  need no token). Group membership never counts, and the game-server user
  `mcsv` never gets this shortcut, even if listed. Operators who use
  `mcsvctl` or `mcsv-console` must be listed by numeric uid, or those tools get
  401.

A request that presents a token is judged by the token alone: a bad, expired or
wrongly signed token is `401` even from root or a listed uid. Only requests
with no token use the unix shortcut. Any relay that runs as root or as a listed
uid (socat, an ssh socket forward, a root container with the socket mounted)
connects as that uid and so hands full access to whoever can reach the relay.

Anyone else gets `401` (no token, or a bad, expired or wrongly signed one).
A valid token without the permission bit the route needs gets `403`. Both are
logged to the journal with the path, peer uid and reason, never the token.
`/service*` and `/backups` answer errors as `{ "error": string }`; the older
`/server*`, `/servers` and `/status` routes keep their empty bodies.

Apache runs as `www-data`, which is in `mcsv-http` but is not listed in
`MCSV_TRUSTED_UIDS`, so web traffic always needs the token. Put the public key
and the trusted uids in `/etc/mcsv_manager/env` (mode 640, group `mcsv-mgr`);
without the key the manager still starts, but only root and listed uids get in.

```bash
MCSV_TRUSTED_UIDS=1000,1001
MCSV_JWT_PUBLIC_KEY=MCowBQYDK2VwAyEAk93DZfic0O/8MgbLN0Avip9dMXbFjSS9m101UOBAGv8=
```

The token's `permission` claim is a bitmask: 1 status, 2 logs, 4 console read,
8 console write, 16 start, 32 stop, 64 restart, 128 backup read, 256 backup
run, 512 update. `cmd` (JSON body
`{"command": "..."}`), `start`, `stop` and `restart` are POST.

## Development

No Rust toolchain is needed on the host, only Docker. `scripts/dev-cargo.sh`
builds `Dockerfile.dev` (Ubuntu 22.04, so release binaries run on any host with
glibc 2.35 or newer) and runs cargo in it:

```bash
scripts/dev-cargo.sh test
scripts/dev-cargo.sh test --features fake-backend
scripts/dev-cargo.sh clippy --all-targets -- -D warnings
scripts/dev-cargo.sh fmt --check
```

The `fake-backend` feature swaps systemd for an in-memory backend and reads
`MCSV_FAKE_LISTEN` (`tcp:127.0.0.1:8080` or `unix:/path/to.sock`) instead of the
socket-activated fd. It is for tests only: never install a binary built with it.

- `tests/fake-smoke.sh`: the fake-backend binary answers over TCP with no D-Bus.
- `tests/apache/run.sh`: real Apache proxying to a unix socket, checking 200,
  401, 403 and the console WebSocket subprotocol.

The Minecraft unit template is [config/systemd/system/minecraft@.service](config/systemd/system/minecraft@.service) (instance `<id>` runs as `minecraft@<id>.service`, stdin FIFO `/run/minecraft/<id>.stdin` created by `minecraft@.socket`). `setup.sh` never overwrites an existing `minecraft@.service`; it writes `minecraft@.service.dist` beside it. Heap and Java are overridable, see the comments in the unit.

\* Further Setup is your own responsibility including instances, Minecraft servers and API.

## Contributing

Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change.

## License

[MIT](LICENSE.txt)