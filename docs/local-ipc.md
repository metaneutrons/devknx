# Local capture protocol (development version 1)

`devknx serve` owns the configured KNXnet/IP connection and SQLite writer.
`devknx status --database PATH` requests one current connection state.
`devknx follow --database PATH` receives that state and subsequent state,
capture, and application-subscriber-lag records as newline-delimited JSON.

The client sends exactly one seven-byte command: `STATUS\n` or `FOLLOW\n`.
The server closes unknown commands. Each JSON record has a `type` discriminator:
`state`, `capture`, or `lagged`. A `state` record has a `value.state`
discriminator (`idle`, `connecting`, `connected`, `waiting_retry`, `stopped`,
`storage_failed`). Capture records carry the monotonic SQLite `id`, receive
timestamp, endpoint, direction, parsed addresses and group-value service, and
exact `raw_cemi` hexadecimal bytes. A `lagged.count` value measures records
missed by this application subscriber; it is not a KNX bus loss count.

On Unix, the socket is `PATH.ipc/control.sock` under a 0700 directory, with
0600 socket permissions. Symlink or shared IPC directories are rejected. On
Windows, the endpoint is a named pipe with a DACL granting access only to the
current process user SID. The pipe requires first-instance creation and rejects
remote clients. This is local transport protection, not remote authentication.

There is no automatic startup, replay request, version negotiation, or write
operation in this development protocol. Durable replay is read from SQLite
with `history` or `export`; frontends must not interpret a transient stream as
complete history. A stable wire contract will be specified before the REST,
MCP, GUI, and TUI adapters rely on it.
