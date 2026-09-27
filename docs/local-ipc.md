# Local capture protocol (development version 3)

`devknx serve` owns the configured KNXnet/IP connection and SQLite writer.
`devknx status --database PATH` requests one current connection state.
`devknx follow --database PATH` receives that state and subsequent state,
capture, router-loss, and application-subscriber-lag records as newline-delimited JSON.

The client sends `STATUS\n` or `FOLLOW\n` for status or the live stream.
For one group operation it sends `OPERATE\n` followed by one bounded JSON line
containing a tagged `read`, `typed_write`, or `raw_write` intent. Pre-encoded
frames are not accepted: the connection owner resolves ETS metadata, validates
the DPT and value, constructs the cEMI frame, and records an audit attempt
before calling the transport. The reply is one JSON line with `type` set to
`operation_result` or `operation_error`. A successful result includes an audit
ID, a sent-capture ID, and exact transmitted cEMI bytes. For a read, `read`
is separately tagged `response` with the matching response frame, or
`no_response` after the requested timeout. A local subscriber lag during
response observation is an error, not a timeout.
The response match uses the requested group address and an observation after
the sent-capture ID. KNX group reads have no transaction ID, so an unrelated
response to the same address during that interval cannot be distinguished.
A connection interruption while waiting is reported as an error rather than
`no_response`.
The server closes unknown commands. Each JSON record has a `type` discriminator:
`state`, `capture`, `routing_lost_message`, or `lagged`. A `state` record has a `value.state`
discriminator (`idle`, `connecting`, `connected`, `waiting_retry`, `stopped`,
`storage_failed`). Capture records carry the monotonic SQLite `id`, receive
timestamp, endpoint, direction, parsed addresses and group-value service, and
exact `raw_cemi` hexadecimal bytes. A `routing_lost_message` record carries a
separate SQLite `id` (or `null` for ephemeral monitoring), timestamp, multicast
endpoint, reporting router's UDP `source`, opaque `device_state`, and
`lost_messages` count. It reports lost KNXnet/IP routing frames at that router;
it is not a general KNX bus-loss measurement. These reports are stored in a
separate SQLite table and read with `devknx router-losses`, not `history` or
the cEMI CSV export. A `lagged` record has `stream` (`capture` or
`routing_loss`) and `count`: records missed by this local IPC subscriber, not
frames reported lost by a router. A `waiting_retry` state denotes a connection
interruption but cannot quantify unobserved telegrams.

On Unix, the socket normally is `PATH.ipc/control.sock` under a 0700 directory,
with 0600 socket permissions. If that name would exceed the portable Unix
socket path limit, it instead lives under a deterministic private
`/tmp/devknx-ipc-<hash>/` directory. Both forms require the IPC directory to
have the same owner as the database. A retained `owner.lock` file in the IPC
directory prevents a second active listener from replacing the socket; do not
delete it while capture runs. Symlink or shared IPC directories are rejected. On
Windows, the endpoint is a named pipe with a DACL granting access only to the
current process user SID. The pipe requires first-instance creation and rejects
remote clients. This is local transport protection, not remote authentication.

There is no automatic startup, replay request, or version negotiation in this
development protocol. Durable replay is read from SQLite
with `history`, `router-losses`, or `export`; frontends must not interpret a transient stream as
complete history. A stable wire contract will be specified before the REST,
MCP, GUI, and TUI adapters rely on it.
