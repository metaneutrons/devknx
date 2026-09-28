# REST API (experimental)

The REST listener is owned by the per-user daemon and disabled by default.
Connect a KNX session explicitly, then enable one listener scoped to that
endpoint. The listener reads the session's capture database; it is not a
second writer. A failed bind does not report REST as enabled. Disconnecting
the selected session disables its listener. Only one REST listener is active
at a time; requests never choose another KNX session implicitly.

The GUI has a clickable REST status in the bottom bar that opens a separate
REST API dialog. The status is refreshed in the background, including after
changes made through the CLI. The dialog contains the bind address, a masked
in-memory bearer token, and the explicit remote-write switch. It does not save
the token. The TUI's `a` key offers a confirmed loopback-only toggle for its
selected session; use the CLI or GUI for an authenticated non-loopback listener.

```sh
devknx connect tunnel://192.0.2.1:3671
devknx rest --enable --endpoint tunnel://192.0.2.1:3671
devknx rest --status
curl http://127.0.0.1:8765/v1/health
curl 'http://127.0.0.1:8765/v1/captures?after=0&limit=100'
curl -N -H 'Last-Event-ID: 42' http://127.0.0.1:8765/v1/events
curl 'http://127.0.0.1:8765/v1/routing-losses?after=0&limit=100'
curl -N http://127.0.0.1:8765/v1/routing-loss-events
curl http://127.0.0.1:8765/v1/openapi.json
devknx rest --disable
```

The complete OpenAPI 3.1 route and schema document is served at
`/v1/openapi.json`. Captures have durable, monotonic IDs. `after` is an
exclusive cursor; `limit` is 1–1000. SSE emits `capture` events with the same
IDs. Reconnect using `Last-Event-ID` or `?after=`, not both. If retention has
removed events between the requested cursor and the next available ID, SSE
first emits `retention_gap` with `after` and `next_available`. It does not
claim to reconstruct discarded events. The stream replays from the durable
database and follows the capture owner's local live feed for wake-ups. If that
feed is temporarily unavailable, it checks the database once per second. An
API restart does not erase the resume cursor.

Router-reported `RoutingLostMessage` diagnostics use a separate history and
SSE cursor. `/v1/routing-losses` pages that history and
`/v1/routing-loss-events` emits `routing_loss` events with resumable IDs and
retention-gap notices. Capture IDs and router-loss IDs must not be mixed.
Router reports are distinct from local subscriber lag and do not establish a
bus-wide loss total. Each capture response additionally has a versioned
`enrichment` object with the active ETS revision, name, hierarchy, declared
DPTs and a decoded value when one DPT permits it. The raw cEMI and stored
capture are unchanged when ETS metadata is updated.

```sh
curl -X POST http://127.0.0.1:8765/v1/operations/preview \
  -H 'Content-Type: application/json' \
  -d '{"address":"1/2/3","dpt":"1.001","value":"true"}'

curl -X POST http://127.0.0.1:8765/v1/operations/typed-write \
  -H 'Content-Type: application/json' \
  -d '{"address":"1/2/3","dpt":"1.001","value":"true"}'
```

Preview does not transmit. Typed writes and group reads use the capture
owner's existing DPT validation, transmission and audit path. The response
records a transport receipt, not confirmation that an actuator changed state.
Operation results include `response_enrichment`: it is `null` for writes and
read timeouts, and contains current ETS metadata and a decoded value when a
matching group-value response is observed and its DPT is unambiguous.
Raw writes are deliberately absent from REST. Operation attempts that pass
preparation are audited with `rest_loopback` or `rest_remote` as their origin.

The listener is disabled unless `rest --enable` succeeds. `rest --status` and
`rest --disable` address only an already-running daemon; they do not launch
one. Status reports the actual bound address and selected endpoint, never the
token. Loopback needs no token by
default; `--token-env VARIABLE` enables bearer authentication there too. A
non-loopback `--bind` fails before listening unless `--token-env` names an
environment variable containing at least 32 bytes. Do not put the token in
the command line or in the repository. Non-loopback writes remain disabled
unless `--allow-remote-writes` is supplied as a separate explicit choice.
The loopback listener rejects cross-site browser requests and unexpected Host
headers.

The listener limits traffic to 600 requests and 60 KNX read/write requests per
rolling minute. At most eight SSE clients can remain connected across both
streams. Excess requests receive JSON `429` with `Retry-After: 60`; failed
authentication is checked before consuming a rate-limit slot. These bounds
protect the local daemon but are not a substitute for network access control.

The built-in HTTP listener does **not** provide TLS. A bearer token on plain
HTTP is visible to anyone who can observe that network path. Do not expose a
non-loopback listener to the public internet; use a trusted private network
or terminate TLS in a controlled reverse proxy. Binding only to loopback and
placing a TLS proxy on the same host is the safer remote-access arrangement.
