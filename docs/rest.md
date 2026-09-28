# REST API (experimental)

`devknx api` is opt-in and reads the selected capture database. It never opens
a second writer. Start `devknx serve tunnel://192.0.2.1:3671` separately for
live status and group operations; history and ETS lookup work when the capture
owner is offline. `--endpoint` selects that connection's default private
capture. Use `--database PATH` on both commands when the owner uses a custom
database path.

```sh
devknx api --endpoint tunnel://192.0.2.1:3671
curl http://127.0.0.1:8765/v1/health
curl 'http://127.0.0.1:8765/v1/captures?after=0&limit=100'
curl -N -H 'Last-Event-ID: 42' http://127.0.0.1:8765/v1/events
curl http://127.0.0.1:8765/v1/openapi.json
```

The complete OpenAPI 3.1 route and schema document is served at
`/v1/openapi.json`. Captures have durable, monotonic IDs. `after` is an
exclusive cursor; `limit` is 1–1000. SSE emits `capture` events with the same
IDs. Reconnect using `Last-Event-ID` or `?after=`, not both. If retention has
removed events between the requested cursor and the next available ID, SSE
first emits `retention_gap` with `after` and `next_available`. It does not
claim to reconstruct discarded events. The stream polls the durable database,
so an API restart does not erase the resume cursor.

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
Raw writes are deliberately absent from REST. Operation attempts that pass
preparation are audited with `rest_loopback` or `rest_remote` as their origin.

The listener is disabled unless `api` is started. Loopback needs no token by
default; `--token-env VARIABLE` enables bearer authentication there too. A
non-loopback `--bind` fails before listening unless `--token-env` names an
environment variable containing at least 32 bytes. Do not put the token in
the command line or in the repository. Non-loopback writes remain disabled
unless `--allow-remote-writes` is supplied as a separate explicit choice.
The loopback listener rejects cross-site browser requests and unexpected Host
headers.

The built-in HTTP listener does **not** provide TLS. A bearer token on plain
HTTP is visible to anyone who can observe that network path. Do not expose a
non-loopback listener to the public internet; use a trusted private network
or terminate TLS in a controlled reverse proxy. Binding only to loopback and
placing a TLS proxy on the same host is the safer remote-access arrangement.
