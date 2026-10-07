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
sudo install -m 755 target/release/mcsv_manager /usr/local/bin/mcsv_manager
```

Basic setup
- setup user & group for safety
- install apache config file, systemd service and socket
```bash
sudo bash scripts/setup.sh
```

Authentication: local service can set host to `bypass` to skip jwt authentication.
For reverse proxy, a short-lived EdDSA token must be signed by trusted service 
(`Authorization: Bearer <jwt>`; the console WebSocket sends it as the second
subprotocol, `mcsv.jwt, <jwt>`).
The config can be found inside `/etc/mcsv_manager/`.

The token's `permission` claim is a bitmask: 1 status, 2 logs, 4 console read,
8 console write, 16 start, 32 stop, 64 restart. `cmd` (JSON body
`{"command": "..."}`), `start`, `stop` and `restart` are POST.

Minecraft server service template is at [config/systemd/system/minecraft@template.service](config/systemd/system/minecraft@template.service)

\* Further Setup is your own responsibility including instances, Minecraft servers and API.

## Contributing

Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change.

## License

[MIT](LICENSE.txt)