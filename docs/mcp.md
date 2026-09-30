# MCP stdio server (experimental)

Start the local MCP server for a KNXnet/IP endpoint's capture. The database
may be created later by `knx_connect`. An
explicit `--database PATH` also works when the owner uses a custom path:

```sh
devknx mcp --endpoint tunnel://192.0.2.1:3671
```

That invocation is read-only with respect to writes: the typed-write tool is
not advertised. To enable it for a deliberately selected group address, use
both switches (repeat `--write-address` for additional exact addresses):

```sh
devknx mcp --endpoint tunnel://192.0.2.1:3671 \
  --allow-writes --write-address 1/2/3
```

Neither `--allow-writes` without an address nor an address without the switch
is accepted. The server checks the allowlist again when the tool executes.

Configure that command in the MCP client. Standard output is reserved for
JSON-RPC; status messages and protocol errors do not become informal output
on that channel. The process exits when its stdio client disconnects.

The server exposes eleven tools by default and a twelfth only with an explicit
write allowlist. Each has a JSON input schema and a structured JSON result:

| Tool | Result |
| --- | --- |
| `knx_discover` | KNXnet/IP gateway addresses, names and raw individual addresses |
| `knx_sessions` | All sessions configured in the current-user daemon, or an empty list when it is not running |
| `knx_connect` | Session created for an explicit endpoint and this MCP server's selected database |
| `knx_disconnect` | Disconnect result for the session associated with this MCP server's selected database only |
| `knx_status` | Capture-owner connection state and active ETS revision |
| `knx_list_captures` | Unfiltered retained capture page and continuation cursor |
| `knx_list_routing_losses` | Separate cursor page of router-reported lost routing frames |
| `knx_search_captures` | Retained capture matches, continuation cursor, scanned count and completion flag |
| `knx_ets_lookup` | Active ETS group metadata for one address |
| `knx_read` | Send receipt and matching response or bounded no-response outcome |
| `knx_write_preview` | Exact unsent cEMI frame and resolved DPT |
| `knx_typed_write` (opt-in) | Audited transport receipt for a DPT-validated typed write to an allowed address |

`knx_search_captures` compares text case-insensitively against endpoint,
direction, source, destination, service, raw cEMI, and current ETS name,
hierarchy, DPT and decoded value. A request returns at
most 100 matches and scans at most 10,000 retained rows. Use `next_after` to
continue when `complete` is false. Search never reads outside the retention
window. Use `knx_list_captures` for unfiltered replay. Both list tools accept
`after` and `limit` (1–100, default 50). Capture and router-loss cursors are
independent: a router report is not local subscriber lag and neither is a
bus-wide loss total.

Capture list and search results add a versioned `enrichment` object while
preserving every raw capture field. It describes the currently imported ETS
revision; changing the ETS catalog never rewrites retained telegrams. No DPT
is guessed when a group is unknown or has multiple declarations.
Read receipts similarly include `response_enrichment` only when a matching
response frame is observed; otherwise the field is `null`.

Session management uses the database selected when the MCP server starts.
`knx_sessions` lists daemon sessions without starting the daemon. `knx_connect`
requires an explicit endpoint, starts the daemon when needed, and assigns the
selected database while requesting a retention limit of 100000 events. With
`mcp --endpoint URL`, the tool endpoint must match that selector; with
`mcp --database PATH`, any explicit endpoint can be bound to the selected file.
`knx_disconnect`
disconnects only the active session for that selected database; it does not
accept an endpoint that could select another session. For example:

```text
knx_sessions: {}
knx_connect: {"endpoint":"tunnel://192.0.2.1:3671"}
knx_disconnect: {}
```

The stdio process and the capture owner communicate over current-user local
IPC. Read and typed-write tools use the same operation preparation,
transmission and durable audit path as CLI and REST. A typed write with no
compatible unambiguous DPT is rejected. There is deliberately no MCP raw-write
tool. The audit origin is `mcp_stdio`; it identifies the adapter, not a
cryptographically authenticated human or model. A successful transport send
does not establish an actuator's resulting state. Preview first and confirm
the intended group address, DPT and value before sending. Starting a writable
MCP process grants its connected client a live bus capability for the listed
addresses; use a separate read-only configuration when that is not intended.

Tool metadata marks session management as state changing and distinguishes
local read-only queries from KNX bus reads, discovery and potentially
destructive writes. One stdio session is limited to 120 tool calls, 12 bus
operations and six discovery calls in any rolling minute;
the limits are shared across concurrent requests within that session. A
limit error does not transmit a KNX frame.
