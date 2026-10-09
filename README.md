# Minecraft Server Manager

A web-based [Minecraft](https://www.minecraft.net/) server manager integrated into [Apache Server](https://httpd.apache.org/) for [Linux](https://kernel.org/) with [Systemd](https://github.com/systemd/systemd).

## Installation

Build the project
```bash
cargo build --release
```

Installation
- install the binary
```bash
sudo bash scripts/install.sh
```

Basic setup
- setup user & group for safety
- install apache config file, systemd service and socket
```bash
sudo bash scripts/setup.sh
```

Auth: the socket is mode 0666, and a request passes if either
- the connecting process is in group `mcsv-mgr` or `adm` and is not the
  game-server user `mcsv` (checked with the kernel's peer credentials, so the
  CLI tools in `src/bin` need no token), or
- it carries a short-lived EdDSA token signed by chulacraft-web
  (`Authorization: Bearer <jwt>`; the console WebSocket sends it as the second
  subprotocol, `mcsv.jwt, <jwt>`).

Apache runs as `www-data`, which must not be in `mcsv-mgr` (setup.sh removes
it), so web traffic always needs the token. Put the public key in `/etc/mcsv_manager/env`
(mode 640, group `mcsv-mgr`); the manager refuses to start without it.

```bash
MCSV_JWT_PUBLIC_KEY=MCowBQYDK2VwAyEAk93DZfic0O/8MgbLN0Avip9dMXbFjSS9m101UOBAGv8=
```

The token's `permission` claim is a bitmask: 1 status, 2 logs, 4 console read,
8 console write, 16 start, 32 stop, 64 restart. `cmd` (JSON body
`{"command": "..."}`), `start`, `stop` and `restart` are POST.

Minecraft server service template is at [config/systemd/system/minecraft@template.service](config/systemd/system/minecraft@template.service)

\* Further Setup is your own responsibility including instances, Minecraft servers and API.

## Contributing

Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change.

## License

[MIT](LICENSE.txt)