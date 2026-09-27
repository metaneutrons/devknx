# ETS group-address import

`devknx` imports two group-address export formats. It does not import full
`.knxproj` projects or infer datapoint types from captured bytes.

```sh
devknx ets-import group-addresses.csv --database captures.sqlite --format csv
devknx ets-import group-addresses.xml --database captures.sqlite --format xml
devknx ets-lookup --database captures.sqlite 1/2/3
```

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
`backup` includes both capture history and ETS revisions. Stop `serve` before
importing because the capture process exclusively owns the database writer.

Multiple declared DPTs are not silently reduced to the first one. A typed
write must use an unambiguous, supported DPT or an explicitly chosen DPT
compatible with the imported declarations. A standard four-column CSV gives
no DPT to validate against; an explicit supported DPT will be required.
Unknown DPTs never authorize a typed write. The M3 operation layer enforces
these rules; import alone does not send telegrams.
