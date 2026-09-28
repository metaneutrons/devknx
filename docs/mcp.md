# MCP stdio server (experimental)

Start the local MCP server for a KNXnet/IP endpoint's existing capture. An
explicit `--database PATH` also works when the owner uses a custom path:

```sh
devknx mcp --endpoint tunnel://192.0.2.1:3671
```

Configure that command in the MCP client. Standard output is reserved for
JSON-RPC; status messages and protocol errors do not become informal output
on that channel. The process exits when its stdio client disconnects.

The server exposes seven tools, each with a JSON input schema and a structured
JSON result:

| Tool | Result |
| --- | --- |
| `knx_discover` | KNXnet/IP gateway addresses, names and raw individual addresses |
| `knx_status` | Capture-owner connection state and active ETS revision |
| `knx_search_captures` | Retained capture matches, continuation cursor, scanned count and completion flag |
| `knx_ets_lookup` | Active ETS group metadata for one address |
| `knx_read` | Send receipt and matching response or bounded no-response outcome |
| `knx_write_preview` | Exact unsent cEMI frame and resolved DPT |
| `knx_typed_write` | Audited transport receipt for a DPT-validated typed write |

`knx_search_captures` compares text case-insensitively against endpoint,
direction, source, destination, service and raw cEMI. A request returns at
most 100 matches and scans at most 10,000 retained rows. Use `next_after` to
continue when `complete` is false. Search never reads outside the retention
window. For historical replay without a text query, use the REST cursor API or
CLI history command.

The stdio process and the capture owner communicate over current-user local
IPC. Read and typed-write tools use the same operation preparation,
transmission and durable audit path as CLI and REST. A typed write with no
compatible unambiguous DPT is rejected. There is deliberately no MCP raw-write
tool. The audit origin is `mcp_stdio`; it identifies the adapter, not a
cryptographically authenticated human or model. A successful transport send
does not establish an actuator's resulting state. Preview first and confirm
the intended group address, DPT and value before sending.
