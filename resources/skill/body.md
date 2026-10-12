# devknx

devknx monitors and controls KNX building installations over KNXnet/IP. A
per-user daemon holds the KNX sessions, records every telegram into a SQLite
capture with a monotonic ID, and serves it to the CLI, this MCP server, a
terminal monitor, a desktop window and a REST interface. Imported ETS project
data gives group addresses their names and datapoint types (DPTs).

## When to use it

- A task involves a KNX installation: finding gateways, watching or searching
  bus traffic, looking up what a group address is, reading a group value, or
  sending one.
- Use it instead of writing KNXnet/IP code or a script against a gateway. The
  daemon already owns the connection and the capture, and other clients may be
  using it.

## How to use it

Use the MCP tools when the devknx MCP server is connected; their names start
with `knx_`. Otherwise use the CLI. An endpoint is written like
`tunnel://192.0.2.1:3671`.

1. `devknx discover` finds KNXnet/IP gateways; `devknx sessions` lists the
   sessions the daemon already holds.
2. `devknx connect ENDPOINT` starts a session. Capture continues until
   `devknx disconnect ENDPOINT`, also when a monitor closes.
3. Read what happened: `devknx history --endpoint ENDPOINT --after ID --limit N`
   pages through the capture, `--filter TEXT` narrows a page, and
   `devknx follow --endpoint ENDPOINT` streams it as JSON lines.
4. Know an address before using it: `devknx ets-lookup --endpoint ENDPOINT 1/2/3`
   gives its name and DPT from the imported ETS data.
5. `devknx read --endpoint ENDPOINT 1/2/3` sends a group read and reports the
   answer, or that none came.

## Ask the user first

A write reaches a live building: lights, blinds, heating, locks.

- Never send a write (`devknx write`, `devknx write-raw`, the
  `knx_typed_write` tool) without the user's explicit agreement to that
  address, value and DPT.
- Show what would be sent first: `devknx write-preview` and
  `knx_write_preview` encode the frame without transmitting it.
- Never guess a DPT. Take it from the ETS lookup, or ask.
- A read also puts a telegram on the bus; ask before reading many addresses.
- `devknx daemon --stop` ends every session, `devknx ets-import` replaces the
  ETS metadata; ask before either.

Capture files can reveal what happens in a building. Do not copy them anywhere
the user did not ask for.

## Reference

Generated from the devknx build that installed this skill:

- [references/cli.md](references/cli.md): every command and option
- [references/mcp.md](references/mcp.md): every MCP tool
- [references/http.md](references/http.md): every REST route
