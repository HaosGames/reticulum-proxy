# Reticulum Proxy [WIP]

A Proxy for bridging Reticulum to the Internet.
You can use it as a SOCKS5 Proxy to access Reticulum destimations from the browser
or forward traffic to in-destinations to IP Ports in your network. This enables
sending any application traffic over reticulum that can also be sent via tcp. For
example HTTP. Tooling that understands these application protocolls can be reused
but traffic flows over reticulum and is proxied to and from tcp. 

## Usage

### SOCKS5 Proxy

```bash
cargo run --bin proxy -- -l 127.0.0.1:1080 -r 127.0.0.1:4242 no-auth
```

Set `127.0.0.1:1080` as your SOCKS5 Proxy in your Browser and enable DNS over SOCKS5.

Then open a destination that is mapped to a http server via a reverse-proxy. For example `http://ffbaa173393b55617485bbcfb248d14c.rns`


### TCP Reverse Proxy

Edit the JSON Mapping file to point a destination with a name to a TCP Port:
```json
{
    "test": {
        "aspects": [
            "test"
        ],
        "forward_to": "127.0.0.1:8080"
    }
}
```

```bash
cargo run --bin reverse-proxy -- -m examples/mappings.json -i examples/identity -c 127.0.0.1:4242
```

The destination hash for that destination is shown in the logs:
```bash
INFO reverse_proxy: listening for Reticulum connections destination.hash="ffbaa173393b55617485bbcfb248d14c" destination.name="test"
```