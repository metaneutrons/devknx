# ETS group-address import

`devknx` imports two group-address export formats. It does not import full
`.knxproj` projects or infer datapoint types from captured bytes.

## GUI and TUI

Select the connection's capture first: save its connection settings or open a
saved capture. In the GUI, choose **Import ETS…** in the toolbar or **File →
Import ETS Group Addresses…** on macOS (`Cmd-I`). Choose the export in the
native file picker, select CSV or XML and, for CSV, UTF-8 or legacy Latin-1.
**Preview import** validates the entire file and shows the destination capture,
address/DPT counts and sample entries. **Replace ETS catalogue** is a separate
confirmation; canceling or closing the preview changes nothing.

In the TUI, press `i`, enter the export path and choose its format and encoding.
The confirmation shows the same bounded summary and samples. Press `y` to
import or `Esc` to cancel; `c` disconnects the selected session when needed.

Disconnect the session using this capture before confirming. The import never
disconnects automatically or stops the daemon or REST listener. An exclusive
writer lease also rejects a racing or external connection. A preview is bound
to the selected database and contains the exact validated metadata snapshot:
changing the source file afterwards cannot alter the confirmed import.
Loaded history, its filter and raw bytes remain in place; names, DPTs and
decodable values refresh immediately after import. Selecting another capture
discards an unconfirmed preview; wait for an ongoing commit to finish before
switching connections.

## CLI and export formats

```sh
devknx ets-import group-addresses.csv --database captures.sqlite --format csv
devknx ets-import group-addresses.xml --database captures.sqlite --format xml
devknx ets-lookup --database captures.sqlite 1/2/3
```

Use `--endpoint tunnel://IP:3671` instead of `--database PATH` to select the
default capture for a connection. Both selectors refer to one capture store;
they cannot be combined in one command.

The format is explicit. CSV is decoded as UTF-8 (an optional BOM is accepted).
For a legacy ISO-8859-1 CSV file, append `--latin1`; this flag is invalid for
XML. GA Export 01 XML must be UTF-8. The standard ETS CSV 3/1 export has four
semicolon-separated columns (`Main`, `Middle`, `Sub`, `Address`) and contains
neither descriptions nor DPTs. The extended nine-column form used by the
earlier KnxMonitor is also accepted, including its description and DPT columns.
XML retains nested range names, descriptions, original address notation, and
every comma-separated DPT declaration. A missing DPT stays missing.

The entire export is validated before the database is opened for writing.
Imports over 16 MiB, more than 100,000 groups, fields over 8 KiB, more than 32
DPTs per group, malformed addresses, duplicate canonical 16-bit addresses,
invalid DPT tokens, unsupported XML namespaces, and XML DTDs fail. No rows
from a rejected import are made active. A successful import creates a new
metadata revision; the previous revision and raw capture bytes remain intact.
`backup` includes both capture history and ETS revisions. Disconnect the selected session before
importing because the capture process exclusively owns the database writer.

Multiple declared DPTs are not silently reduced to the first one. A typed
write must use an unambiguous, supported DPT or an explicitly chosen DPT
compatible with the imported declarations. A standard four-column CSV gives
no DPT to validate against; an explicit supported DPT will be required.
Unknown DPTs never authorize a typed write. The first operation layer supports
a conservative set of typed identifiers: 1.001, 1.002, 1.006, 4.001,
5.001, 5.003, 5.010, 7.001, 8.001, 9.001, 9.004, 12.001, 13.001,
14.056, 16.000, 16.001, 17.001, 18.001, and 29.010. Other imported DPT
declarations remain visible but cannot be transmitted as typed values yet.
The operation layer enforces
these rules again inside the running connection owner; import alone does not
send telegrams.
