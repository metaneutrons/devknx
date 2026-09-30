# REST API (experimental)

The REST listener is owned by the per-user daemon and disabled by default.
It can start without a KNX endpoint or an active session. A failed bind does
not report REST as enabled. Disconnecting a session leaves REST available for
reconnection. Only one REST listener is active at a time. Each data request
selects an endpoint explicitly unless a default was selected when REST started;
there is no implicit choice among multiple sessions.

The GUI has a clickable REST status in the bottom bar that opens a separate
REST API dialog. An endpoint in Connection Settings is optional and becomes
the listener's default target if present. The status is refreshed in the background, including after
changes made through the CLI. The dialog contains the bind address, a masked
in-memory bearer token, and the explicit remote-write switch. It does not save
the token. The TUI's `a` key offers a confirmed loopback-only toggle for its
selected endpoint, even while disconnected. Use the CLI or GUI for an
authenticated non-loopback listener.

```sh
devknx rest --enable
devknx rest --status
curl http://127.0.0.1:8765/v1/health
curl http://127.0.0.1:8765/v1/sessions
curl -X POST http://127.0.0.1:8765/v1/sessions \
  -H 'Content-Type: application/json' \
  -d '{"endpoint":"tunnel://192.0.2.1:3671"}'
curl 'http://127.0.0.1:8765/v1/captures?endpoint=tunnel://192.0.2.1:3671&after=0&limit=100'
curl -N -H 'Last-Event-ID: 42' 'http://127.0.0.1:8765/v1/events?endpoint=tunnel://192.0.2.1:3671'
curl 'http://127.0.0.1:8765/v1/routing-losses?endpoint=tunnel://192.0.2.1:3671&after=0&limit=100'
curl -N 'http://127.0.0.1:8765/v1/routing-loss-events?endpoint=tunnel://192.0.2.1:3671'
curl -X DELETE 'http://127.0.0.1:8765/v1/sessions?endpoint=tunnel://192.0.2.1:3671'
curl http://127.0.0.1:8765/v1/openapi.json
devknx rest --disable
```

`GET /v1/sessions` lists active daemon sessions. `POST /v1/sessions` requires
an endpoint in its JSON body and may set `max_events`; `DELETE /v1/sessions`
requires an endpoint query parameter. `GET /v1/connection?endpoint=...`
reports the chosen session and owner state. The `POST` and `DELETE`
`/v1/connection` shortcuts operate only when a default endpoint was selected
with `rest --enable --endpoint URL`. The default also allows data and operation
requests to omit `endpoint`. Without a default, specify `endpoint` on capture,
ETS, and SSE query strings and in typed-write, preview, and read JSON bodies.
`rest --enable` starts the daemon when necessary but does not connect to KNX.
Use `--database PATH` with `--endpoint URL` to override that default target's
capture path. HTTP clients cannot supply arbitrary database paths; sessions
otherwise use endpoint-derived paths. Before the first connection,
`/v1/health` reports `capture_owner: null` and `ets_revision: null`.

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
  -d '{"endpoint":"tunnel://192.0.2.1:3671","address":"1/2/3","dpt":"1.001","value":"true"}'

curl -X POST http://127.0.0.1:8765/v1/operations/typed-write \
  -H 'Content-Type: application/json' \
  -d '{"endpoint":"tunnel://192.0.2.1:3671","address":"1/2/3","dpt":"1.001","value":"true"}'
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
one. Status reports the actual bound address and optional default endpoint, never the
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
