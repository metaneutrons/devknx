# M2 real-hardware qualification

This procedure supplies the evidence that loopback tunnel and router-path
fixtures cannot provide. It does not authorize a group-value write or any
change to a production KNX installation. Run it only on a network and gateway
the operator has identified as in scope. Do not commit raw building traffic,
gateway credentials, ETS exports, or unredacted capture databases.

## Record the candidate and environment

Record the tested Git revision, `devknx --version`, OS and architecture, gateway
make/model and firmware, tunnel address, multicast group/port, network
interface, and test window in the [M2 issue](https://github.com/metaneutrons/devknx/issues/5).
Use non-sensitive descriptions for public evidence. If routing and tunneling
need different gateways or networks, identify each separately. A loopback
`DeviceServer` or UDP packet injected into a local socket is not hardware
evidence.

## Passive tunnel capture

Use a new private database path and replace the example endpoint with the
operator-approved gateway. Start `serve` in one terminal:

```sh
devknx serve tunnel://192.0.2.1:3671 --database ./m2-tunnel.sqlite
```

In another terminal, run `devknx status --database ./m2-tunnel.sqlite` and
`devknx follow --database ./m2-tunnel.sqlite`. Record the transition to
`connected` and observe naturally occurring bus traffic. `follow` is live and
can lag; SQLite is the durable source. Confirm that each observed live ID
appears in `devknx history --database ./m2-tunnel.sqlite --after 0` with the
same raw cEMI bytes. Check source, destination, direction, and group service
against independently known traffic. Record separately whether read, response,
and write telegrams were actually seen. Do not infer a missing service from a
quiet capture or manufacture a frame and call it physical-bus evidence.

Stop the process, restart it against the same database and gateway, and observe
another naturally occurring telegram. Confirm that the old capture remains,
the new ID is higher, and `devknx export --database ./m2-tunnel.sqlite` and
`devknx backup --database ./m2-tunnel.sqlite --output ./m2-tunnel-backup.sqlite`
read the committed data. Check the snapshot with
`devknx history --database ./m2-tunnel-backup.sqlite --after 0`. The backup
destination must not already exist.

## Passive physical routing capture

Repeat with a separate database and the approved physical multicast group:

```sh
devknx serve router://224.0.23.12:3671 --database ./m2-router.sqlite
```

The endpoint must receive routing indications from a real KNXnet/IP router
over the selected network interface, not a local unicast injection into the
same UDP port. Check `status`, `follow`, durable history, and raw cEMI as above.
Also check `devknx router-losses --database ./m2-router.sqlite`. If a real
router emits `RoutingLostMessage` (`0x0531`) during the passive window, confirm
that `follow` emits a distinct `routing_lost_message` with the reporting
router source, device state, and count, and that the same ID is durable and
present in a restored backup. Do not induce packet loss on a production bus
to manufacture this diagnostic. Record explicitly when none was observed;
synthetic parser/IPC tests are not physical-router evidence for `0x0531`.
If the host has multiple interfaces, record which interface carried the
multicast packets. A successful `connected` state alone proves only that the
multicast socket joined; it does not prove telegram delivery.

## Evidence and decision

Record the exact revision and CI run, platform, gateway identities, timestamps,
connection states, number of captured frames, observed group-service categories,
restart IDs, backup/readback result, and whether routing packets came from the
physical network. Keep sensitive raw captures local; if artifacts are retained,
record a SHA-256 digest and private location rather than publishing the bytes.
Record every missing service category or topology limitation explicitly.

`lagged.count` measures application subscriber loss, not KNX bus loss. Router
`lost_messages` counts KNXnet/IP routing frames the reporting router says it
lost; it is not a general KNX bus-loss count. `waiting_retry` records a
connection interruption with no inferred loss count. Close M2 only after the
plan criteria and this real-hardware gate have evidence, or after an explicit
reviewed change to the normative plan.
