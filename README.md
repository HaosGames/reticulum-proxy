# Reticulum Proxy

A Proxy for bridging Reticulum to the Internet.
You can use it as a SOCKS5 Proxy to access Reticulum destimations from the browser
or forward traffic to in-destinations to IP Ports in your network. This enables
sending any application traffic over reticulum that can also be sent via tcp. For
example HTTP. Tooling that understands these application protocolls can be reused
but traffic flows over reticulum and is proxied to and from tcp. 