# Local control and capture protocols (development)

One per-user `devknx daemon` owns zero or more configured KNXnet/IP sessions.
Starting it alone does not connect to KNX. Each connected session owns one
SQLite writer and retains a database-scoped capture IPC endpoint. A separate
current-user control endpoint accepts bounded JSON-line requests: `ping`,
`list`, `connect`, `disconnect`, `disconnect_scoped`, `rest_enable`, `rest_disable`, `rest_status`,
and `stop`. Connect identifies a canonical endpoint and optional database
override. Disconnect identifies one endpoint; `disconnect_scoped` atomically
selects the session by capture database. Stop ends all sessions and the daemon.
REST status omits its bearer token. CLI offline commands and status queries do
not auto-start the daemon; endpoint-selected live operations, explicit Connect,
and explicit REST enable may do so. REST enable alone does not connect to KNX.

`devknx status --endpoint URL` requests one current connection state.
`devknx follow --endpoint URL` receives that state and subsequent state,
capture, router-loss, and application-subscriber-lag records as newline-delimited JSON.
The selector can instead be `--database PATH` when the owner uses a custom
capture path. CLI commands do not infer the target from the GUI's last-used
connection.
The GUI and TUI can launch that same owner on explicit Connect and leave it
running after the window or terminal interface closes.

The client sends `STATUS\n` or `FOLLOW\n` for status or the live stream.
Legacy per-database `STOP\n` is not enabled for daemon sessions. Session
lifecycle uses the control endpoint so a stopped session cannot terminate
other connections or the daemon. REST lists and controls explicit endpoints over
`/v1/sessions`; `/v1/connection` is a shortcut for a listener default. MCP can
connect an explicit endpoint to its selected database and disconnect only that
database's active session.
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
`storage_failed`). The optional `configured_endpoint` on a state record
identifies the owner's target even while it is connecting or retrying; GUI and
TUI check it before attaching or claiming that a new owner started. Capture
records carry the monotonic SQLite `id`, receive
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

On Unix, a session socket normally is `PATH.ipc/control.sock` under a 0700 directory,
with 0600 socket permissions. If that name would exceed the portable Unix
socket path limit, it instead lives under a deterministic private
`/tmp/devknx-ipc-<hash>/` directory. Both forms require the IPC directory to
have the same owner as the database. A retained `owner.lock` file in the IPC
directory prevents a second active listener from replacing the socket; do not
delete it while capture runs. Symlink or shared IPC directories are rejected. On
Windows, the endpoint is a named pipe with a DACL granting access only to the
current process user SID. The pipe requires first-instance creation and rejects
remote clients. This is local transport protection, not remote authentication.

The daemon control socket has a stable identity for the current user and
application-data directory, independent of capture database paths. Its Unix
directory is private and protected by an owner lease; Windows uses a
SID-restricted named pipe whose name also identifies the application-data
directory. Clients may spawn `devknx daemon`
on demand, but the daemon connects to KNX only after a scoped Connect request.
There is no replay request or version negotiation in this development protocol.
Durable replay is read from SQLite
with `history`, `router-losses`, or `export`; frontends must not interpret a transient stream as
complete history. A stable external wire contract is not yet promised.
