# Dialect oracle snippets

Locked header / table snippets used by `crates/protocol` dialect tests.
The full wire write-up is `../replica.md`.

| File | Why |
|---|---|
| `paths.h` | URL paths |
| `clients.c` / `clients.h` | quirk table (order matters) |
| `scanner.h` | object IDs `64` / `1` / `2` |
| `containers.c` | recent views and PlaysForSure/Samsung aliases |
| `scanner-classification.c` / `utils-media.c` | scanner classes and admitted extensions |
| `httpersist.h` | Keep-Alive rule |
| `upnpsoap.h` | DIDL namespaces |
| `upnpsoap-faults.c` | SOAP fault shape and reference error codes |
| `upnpglobalvars.h` | advertised protocol-info entries |
| `upnpdescgen-root.c` | rootDesc and SCPD action tables |
| `minissdp-wire.c` | alive, byebye, and M-SEARCH packet shapes |
| `w3c_date.c` | date normalize |

These files are third-party C excerpts kept for literal matching.
Do not treat them as the process model to port.
